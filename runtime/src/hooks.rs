//! Running plugin tool hooks, inline with the tool call they guard.
//!
//! Hooks run here rather than server-side for three reasons: this is the only
//! place the plugin files exist (every vendor's job ends at materialising
//! `plugins_dir`), a hook wrapping a call the runtime is already handling costs
//! no extra round-trip, and an inline hook inherits the existing `CancelCall`
//! path — a user hitting Stop interrupts a slow hook for free.
//!
//! What each hook did rides back on the tool response as a [`HookRecord`], so
//! the server can journal it and the user can see what a plugin changed.

use horsie_models::runtime::{HookRecord, ToolCall, ToolError, ToolOutput, ToolResult};
use horsie_support::plugin::hooks::{HookDecl, HookEvent, matcher_applies};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::state::RuntimeState;
use crate::workspace::WorkspaceRegistry;

/// Default per-hook budget when a declaration does not set one.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on any single before/after payload recorded for the UI. A hook that
/// rewrites a large file write must not bloat the session journal.
const RECORD_CLAMP: usize = 8_000;

/// The agent-facing name of a tool call.
///
/// The wire union is tagged in PascalCase, but the LLM, the matcher tables and
/// the user all see snake_case. Exhaustive on purpose: adding a tool fails to
/// compile until it is named here.
fn tool_name(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Bash(_) => "bash",
        ToolCall::ReadFile(_) => "read_file",
        ToolCall::WriteFile(_) => "write_file",
        ToolCall::FindAndReplace(_) => "find_and_replace",
        ToolCall::ReplaceLines(_) => "replace_lines",
        ToolCall::ListFiles(_) => "list_files",
        ToolCall::Glob(_) => "glob",
        ToolCall::Grep(_) => "grep",
        ToolCall::SetWorkingDir(_) => "set_working_dir",
        ToolCall::SetEnv(_) => "set_env",
    }
}

fn clamp(s: &str) -> String {
    s.chars().take(RECORD_CLAMP).collect()
}

/// A blank record for a hook that is about to run.
fn record(plugin: &str, event: HookEvent, tool: &str) -> HookRecord {
    HookRecord {
        plugin: plugin.to_string(),
        event: event.as_str().to_string(),
        tool: tool.to_string(),
        duration_ms: 0,
        blocked: false,
        reason: None,
        failed: false,
        input_before: None,
        input_after: None,
        output_before: None,
        output_after: None,
        additional_context: None,
        system_message: None,
    }
}

/// Every declaration for `event` whose matcher selects `tool`, with the plugin
/// root and name it came from, in stable plugin order.
fn matching(plugins_dir: &Path, event: HookEvent, tool: &str) -> Vec<(PathBuf, String, HookDecl)> {
    let mut out = Vec::new();
    for plugin_root in crate::plugins::plugin_dirs(plugins_dir) {
        let Ok(hooks) = horsie_support::plugin::hooks::read(&plugin_root) else {
            continue;
        };
        let name = plugin_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for decl in hooks.decls {
            if decl.event == event && matcher_applies(decl.matcher.as_deref(), tool) {
                out.push((plugin_root.clone(), name.clone(), decl));
            }
        }
    }
    out
}

/// Dispatch a tool call, running any `PreToolUse` and `PostToolUse` hooks that
/// select it.
///
/// Returns the result the server should relay plus a record of every hook that
/// ran. With no plugin library, or no matching declaration, this is exactly
/// [`crate::tools::dispatch`] with an empty record list.
pub async fn dispatch_with_hooks(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    agent: &str,
    call: ToolCall,
) -> (ToolResult, Vec<HookRecord>) {
    let Some(plugins_dir) = registry.plugins_dir() else {
        return (crate::tools::dispatch(registry, state, agent, call).await, Vec::new());
    };
    let name = tool_name(&call);
    let hook_path = registry.hook_path();
    let mut records = Vec::new();

    // --- PreToolUse ---
    let pre = matching(plugins_dir, HookEvent::PreToolUse, name);
    let mut call = call;
    for (root, plugin, decl) in pre {
        let input = call_input(&call);
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": name,
            "tool_input": input,
        })
        .to_string();
        let (outcome, mut rec) = run_one(&root, &plugin, name, &decl, hook_path, &payload).await;

        if outcome.blocked || outcome.failed {
            rec.blocked = true;
            records.push(rec);
            // Fail closed: a guard that could not run is not a guard. This is a
            // deliberate divergence from Claude Code, and it applies to
            // `PreToolUse` alone — every other event runs after the fact.
            let reason = deny_reason(&outcome, &plugin, name);
            return (ToolResult::Err(ToolError { reason }), records);
        }
        if let Some(updated) = outcome.updated_input.as_deref()
            && let Ok(v) = serde_json::from_str::<Value>(updated)
            && let Some(rewritten) = with_input(&call, v.clone())
        {
            rec.input_before = Some(clamp(&input.to_string()));
            rec.input_after = Some(clamp(&v.to_string()));
            call = rewritten;
        }
        records.push(rec);
    }

    let mut result = crate::tools::dispatch(registry, state, agent, call.clone()).await;

    // --- PostToolUse ---
    let post = matching(plugins_dir, HookEvent::PostToolUse, name);
    for (root, plugin, decl) in post {
        let (response, is_error) = match &result {
            ToolResult::Ok(o) => (o.stdout.clone(), false),
            ToolResult::Err(e) => (e.reason.clone(), true),
        };
        let payload = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": name,
            "tool_input": call_input(&call),
            "tool_response": response,
            "is_error": is_error,
        })
        .to_string();
        let (outcome, mut rec) = run_one(&root, &plugin, name, &decl, hook_path, &payload).await;

        // The tool already ran: a failure here is recorded but never fatal, and
        // never rewrites a result the hook could not read.
        if !outcome.failed && let ToolResult::Ok(out) = &mut result {
            if let Some(replacement) = &outcome.updated_tool_output {
                rec.output_before = Some(clamp(&out.stdout));
                rec.output_after = Some(clamp(replacement));
                out.stdout = replacement.clone();
            }
            if let Some(ctx) = &outcome.additional_context {
                out.stdout = format!("{}\n\n{ctx}", out.stdout);
            }
        }
        records.push(rec);
    }

    (result, records)
}

/// Why a `PreToolUse` outcome denied the call, phrased for the model.
fn deny_reason(outcome: &Outcome, plugin: &str, tool: &str) -> String {
    if outcome.blocked {
        match &outcome.reason {
            Some(r) => format!("plugin '{plugin}' blocked '{tool}': {r}"),
            None => format!("plugin '{plugin}' blocked '{tool}'"),
        }
    } else {
        format!("a '{plugin}' hook for '{tool}' could not be run, so the call was denied")
    }
}

/// The tool's input object — what the LLM sent, and what a hook sees and may
/// rewrite. The wire form is `{"tool": "...", "value": {...}}`.
fn call_input(call: &ToolCall) -> Value {
    serde_json::to_value(call)
        .ok()
        .and_then(|v| v.get("value").cloned())
        .unwrap_or(Value::Null)
}

/// Rebuild a call with a hook's replacement input, or `None` when the
/// replacement does not deserialize — a hook must not be able to corrupt a call
/// into something the runtime cannot run.
fn with_input(call: &ToolCall, input: Value) -> Option<ToolCall> {
    let mut wire = serde_json::to_value(call).ok()?;
    let obj = wire.as_object_mut()?;
    obj.insert("value".to_string(), input);
    serde_json::from_value(wire).ok()
}

/// What one hook decided, before it is folded into a record.
#[derive(Default)]
struct Outcome {
    blocked: bool,
    reason: Option<String>,
    failed: bool,
    updated_input: Option<String>,
    updated_tool_output: Option<String>,
    additional_context: Option<String>,
    system_message: Option<String>,
}

/// Run one hook and fold its output into an outcome and a record.
async fn run_one(
    plugin_root: &Path,
    plugin: &str,
    tool: &str,
    decl: &HookDecl,
    hook_path: &[PathBuf],
    payload: &str,
) -> (Outcome, HookRecord) {
    let mut rec = record(plugin, decl.event, tool);
    let mut outcome = Outcome::default();

    let command = decl
        .command
        .replace("${CLAUDE_PLUGIN_ROOT}", &plugin_root.to_string_lossy());
    let timeout = decl.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs);

    let started = Instant::now();
    let run =
        crate::plugins::run_hook_raw(plugin_root, &command, hook_path, payload, timeout).await;
    rec.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    match run.code {
        Some(0) => merge(&run.stdout, &mut outcome),
        Some(2) => {
            outcome.blocked = true;
            let reason = run.stderr.trim();
            if !reason.is_empty() {
                outcome.reason = Some(reason.to_string());
            }
        }
        _ => outcome.failed = true,
    }

    rec.blocked = outcome.blocked;
    rec.failed = outcome.failed;
    rec.reason = outcome.reason.clone();
    rec.additional_context = outcome.additional_context.as_deref().map(clamp);
    rec.system_message = outcome.system_message.clone();
    (outcome, rec)
}

/// Interpret a successful hook's stdout per Claude Code's contract.
fn merge(stdout: &str, out: &mut Outcome) {
    let Ok(json) = serde_json::from_str::<Value>(stdout) else {
        // Not JSON: Claude Code treats plain stdout as injected context.
        let text = stdout.trim();
        if !text.is_empty() {
            out.additional_context = Some(text.to_string());
        }
        return;
    };
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);

    if let Some(msg) = s(json.get("systemMessage")) {
        out.system_message = Some(msg);
    }
    if json.get("decision").and_then(Value::as_str) == Some("block") {
        out.blocked = true;
        out.reason = s(json.get("reason"));
    }

    let Some(hso) = json.get("hookSpecificOutput") else {
        return;
    };
    if let Some(ctx) = s(hso.get("additionalContext")) {
        out.additional_context = Some(ctx);
    }
    if let Some(input) = hso.get("updatedInput") {
        out.updated_input = Some(input.to_string());
    }
    if let Some(output) = hso.get("updatedToolOutput") {
        out.updated_tool_output = Some(
            output
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| output.to_string()),
        );
    }
    match hso.get("permissionDecision").and_then(Value::as_str) {
        Some("deny") => {
            out.blocked = true;
            out.reason = s(hso.get("permissionDecisionReason")).or(out.reason.take());
        }
        // horsie has no permission prompt and runs unattended sessions, so
        // there is nobody to ask. Deliberate divergence from Claude Code.
        Some(other @ ("ask" | "defer")) => {
            tracing::info!(
                decision = other,
                "hook asked for approval; horsie has no permission prompt, allowing"
            );
        }
        _ => {}
    }
}

/// A successful, unmodified tool output — used when a hook denies before the
/// tool ever runs and the caller still wants a shaped result.
#[allow(dead_code)]
fn empty_output() -> ToolOutput {
    ToolOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
    }
}
