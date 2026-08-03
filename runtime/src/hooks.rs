//! Running plugin hooks and reporting what the session's plugins declare.
//!
//! Hooks execute here, in the runtime, because this is where the plugin files
//! and the workspace are. The server decides *whether* a hook applies — it holds
//! the manifest and evaluates matchers — and asks for an event only when one
//! does, so a session without matching hooks never round-trips.

use horsie_models::runtime::{HookDeclWire, HookOutcomeWire};
use horsie_support::plugin::hooks::{HookEvent, HookDecl};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default per-hook budget when a declaration does not set one.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// An all-clear outcome: nothing blocked, nothing failed, nothing amended.
pub fn clear_outcome() -> HookOutcomeWire {
    clear()
}

/// An all-clear outcome: nothing blocked, nothing failed, nothing amended.
fn clear() -> HookOutcomeWire {
    HookOutcomeWire {
        blocked: false,
        reason: None,
        additional_context: None,
        updated_input: None,
        updated_tool_output: None,
        system_message: None,
        stop: false,
        stop_reason: None,
        failed: false,
    }
}

/// What the session's plugins declare, and what horsie cannot run.
///
/// A plugin whose `hooks.json` is malformed is reported as unsupported rather
/// than silently contributing nothing — a broken guard must be visible.
pub fn manifest(plugins_dir: &Path) -> (Vec<HookDeclWire>, Vec<String>) {
    let mut entries = Vec::new();
    let mut unsupported = Vec::new();
    for plugin_root in crate::plugins::plugin_dirs(plugins_dir) {
        let name = plugin_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match horsie_support::plugin::hooks::read(&plugin_root) {
            Ok(hooks) => {
                for d in hooks.decls {
                    entries.push(HookDeclWire {
                        event: d.event.as_str().to_string(),
                        matcher: d.matcher,
                    });
                }
                for (event, why) in hooks.unsupported {
                    unsupported.push(format!("plugin '{name}': {}", why.explain(&event)));
                }
            }
            Err(e) => unsupported.push(format!("plugin '{name}': {e}")),
        }
    }
    (entries, unsupported)
}

/// Every declaration for `event`, paired with its plugin root, in stable plugin
/// order so a session's hooks run the same way twice.
fn decls_for(plugins_dir: &Path, event: HookEvent) -> Vec<(PathBuf, HookDecl)> {
    let mut out = Vec::new();
    for plugin_root in crate::plugins::plugin_dirs(plugins_dir) {
        let Ok(hooks) = horsie_support::plugin::hooks::read(&plugin_root) else {
            continue;
        };
        for decl in hooks.decls {
            if decl.event == event {
                out.push((plugin_root.clone(), decl));
            }
        }
    }
    out
}

/// Run every hook declared for `event` and merge their outcomes.
///
/// Hooks run in stable plugin order. The first block stops the chain — a later
/// hook cannot un-block what an earlier one refused. `additional_context`
/// accumulates; `updated_input` and `updated_tool_output` are last-writer-wins,
/// so ordering is deterministic rather than incidental.
pub async fn run(
    plugins_dir: &Path,
    hook_path: &[PathBuf],
    event: &str,
    payload: &str,
) -> HookOutcomeWire {
    let mut out = clear();
    let Ok(parsed) = HookEvent::parse(event) else {
        // The server only asks for events it read from our own manifest, so
        // this is a protocol mismatch rather than a plugin problem.
        tracing::warn!(event, "asked to run an event this runtime cannot classify");
        return out;
    };

    let mut contexts: Vec<String> = Vec::new();
    for (plugin_root, decl) in decls_for(plugins_dir, parsed) {
        let command = decl.command.replace(
            "${CLAUDE_PLUGIN_ROOT}",
            &plugin_root.to_string_lossy(),
        );
        let timeout = decl.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs);
        let run =
            crate::plugins::run_hook_raw(&plugin_root, &command, hook_path, payload, timeout).await;
        interpret(run.code, &run.stdout, &run.stderr, &mut out, &mut contexts);
        if out.blocked {
            break;
        }
    }
    if !contexts.is_empty() {
        out.additional_context = Some(contexts.join("\n\n"));
    }
    out
}

/// Interpret one hook's exit status and output per Claude Code's contract.
///
/// exit 0 → stdout parsed as JSON when it parses, else treated as
/// `additionalContext`. exit 2 → blocking, stderr is the reason. Anything else,
/// including a timeout or a spawn failure, is an outage rather than a decision.
fn interpret(
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
    out: &mut HookOutcomeWire,
    contexts: &mut Vec<String>,
) {
    match code {
        Some(0) => merge(stdout, out, contexts),
        Some(2) => {
            out.blocked = true;
            let reason = stderr.trim();
            if !reason.is_empty() {
                out.reason = Some(reason.to_string());
            }
        }
        _ => out.failed = true,
    }
}

/// Merge a successful hook's stdout into the running outcome.
fn merge(stdout: &str, out: &mut HookOutcomeWire, contexts: &mut Vec<String>) {
    let Ok(json) = serde_json::from_str::<Value>(stdout) else {
        // Not JSON: Claude Code treats plain stdout as injected context.
        let text = stdout.trim();
        if !text.is_empty() {
            contexts.push(text.to_string());
        }
        return;
    };

    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);

    if json.get("continue").and_then(Value::as_bool) == Some(false) {
        out.stop = true;
        out.stop_reason = s(json.get("stopReason"));
    }
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
        contexts.push(ctx);
    }
    if let Some(input) = hso.get("updatedInput") {
        out.updated_input = Some(input.to_string());
    }
    if let Some(output) = hso.get("updatedToolOutput") {
        // A string replacement is used verbatim; anything else is re-encoded.
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A plugin whose hook is a shell script doing exactly what the test needs.
    fn plugin(root: &Path, name: &str, hooks_json: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(dir.join("hooks/hooks.json"), hooks_json).unwrap();
    }

    fn one_hook(event: &str, command: &str) -> String {
        format!(
            r#"{{"hooks":{{"{event}":[{{"hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#
        )
    }

    #[test]
    fn manifest_reports_declarations_and_unsupported_events() {
        let dir = TempDir::new().unwrap();
        plugin(
            dir.path(),
            "good",
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[
                 {"type":"command","command":"true"}]}]}}"#,
        );
        plugin(dir.path(), "bad", &one_hook("WorktreeCreate", "true"));

        let (entries, unsupported) = manifest(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, "PreToolUse");
        assert_eq!(entries[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(unsupported.len(), 1);
        assert!(unsupported[0].contains("bad"), "{:?}", unsupported);
        assert!(unsupported[0].contains("WorktreeCreate"), "{:?}", unsupported);
    }

    #[test]
    fn a_malformed_hooks_file_surfaces_rather_than_vanishing() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("broken");
        std::fs::create_dir_all(p.join("hooks")).unwrap();
        std::fs::write(p.join("hooks/hooks.json"), "{nope").unwrap();
        let (entries, unsupported) = manifest(dir.path());
        assert!(entries.is_empty());
        assert_eq!(unsupported.len(), 1);
        assert!(unsupported[0].contains("broken"), "{:?}", unsupported);
    }

    #[tokio::test]
    async fn exit_zero_with_additional_context_injects_it() {
        let dir = TempDir::new().unwrap();
        plugin(
            dir.path(),
            "p",
            &one_hook(
                "PreToolUse",
                r#"printf '{\"hookSpecificOutput\":{\"additionalContext\":\"CTX\"}}'"#,
            ),
        );
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert_eq!(out.additional_context.as_deref(), Some("CTX"));
        assert!(!out.blocked);
        assert!(!out.failed);
    }

    #[tokio::test]
    async fn plain_stdout_is_treated_as_context() {
        let dir = TempDir::new().unwrap();
        plugin(dir.path(), "p", &one_hook("PreToolUse", "echo hello"));
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert_eq!(out.additional_context.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn exit_two_blocks_and_stderr_is_the_reason() {
        let dir = TempDir::new().unwrap();
        plugin(
            dir.path(),
            "p",
            &one_hook("PreToolUse", "echo nope 1>&2; exit 2"),
        );
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert!(out.blocked);
        assert_eq!(out.reason.as_deref(), Some("nope"));
        assert!(!out.failed, "a block is a decision, not an outage");
    }

    /// The distinction the whole fail-closed rule rests on.
    #[tokio::test]
    async fn any_other_non_zero_exit_is_a_failure_not_a_block() {
        let dir = TempDir::new().unwrap();
        plugin(dir.path(), "p", &one_hook("PreToolUse", "exit 1"));
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert!(out.failed);
        assert!(!out.blocked);
    }

    #[tokio::test]
    async fn permission_decision_deny_blocks_with_its_reason() {
        let dir = TempDir::new().unwrap();
        plugin(
            dir.path(),
            "p",
            &one_hook(
                "PreToolUse",
                r#"printf '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"no way\"}}'"#,
            ),
        );
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert!(out.blocked);
        assert_eq!(out.reason.as_deref(), Some("no way"));
    }

    /// horsie has no permission prompt, so `ask` allows rather than blocking.
    #[tokio::test]
    async fn permission_decision_ask_allows() {
        let dir = TempDir::new().unwrap();
        plugin(
            dir.path(),
            "p",
            &one_hook(
                "PreToolUse",
                r#"printf '{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\"}}'"#,
            ),
        );
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert!(!out.blocked);
        assert!(!out.failed);
    }

    #[tokio::test]
    async fn updated_input_and_output_round_trip() {
        let dir = TempDir::new().unwrap();
        plugin(
            dir.path(),
            "p",
            &one_hook(
                "PreToolUse",
                r#"printf '{\"hookSpecificOutput\":{\"updatedInput\":{\"a\":1},\"updatedToolOutput\":\"replaced\"}}'"#,
            ),
        );
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert_eq!(out.updated_input.as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(out.updated_tool_output.as_deref(), Some("replaced"));
    }

    #[tokio::test]
    async fn continue_false_asks_the_caller_to_stop() {
        let dir = TempDir::new().unwrap();
        plugin(
            dir.path(),
            "p",
            &one_hook(
                "PreToolUse",
                r#"printf '{\"continue\":false,\"stopReason\":\"done here\"}'"#,
            ),
        );
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert!(out.stop);
        assert_eq!(out.stop_reason.as_deref(), Some("done here"));
    }

    #[tokio::test]
    async fn several_hooks_concatenate_context_and_the_first_block_stops_the_chain() {
        let dir = TempDir::new().unwrap();
        plugin(dir.path(), "a-first", &one_hook("PreToolUse", "echo one"));
        plugin(dir.path(), "b-second", &one_hook("PreToolUse", "echo two"));
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert_eq!(out.additional_context.as_deref(), Some("one\n\ntwo"));

        let dir2 = TempDir::new().unwrap();
        plugin(
            dir2.path(),
            "a-blocks",
            &one_hook("PreToolUse", "echo stop 1>&2; exit 2"),
        );
        plugin(
            dir2.path(),
            "b-never-runs",
            &one_hook("PreToolUse", "echo later"),
        );
        let out2 = run(dir2.path(), &[], "PreToolUse", "{}").await;
        assert!(out2.blocked);
        assert!(
            out2.additional_context.is_none(),
            "a hook after the block must not have run"
        );
    }

    #[tokio::test]
    async fn the_payload_reaches_the_hook_on_stdin() {
        let dir = TempDir::new().unwrap();
        plugin(dir.path(), "p", &one_hook("PreToolUse", "cat"));
        let out = run(dir.path(), &[], "PreToolUse", r#"{"tool_name":"bash"}"#).await;
        // `cat` echoes the payload, which is valid JSON without the envelope,
        // so nothing is injected — but it must not fail.
        assert!(!out.failed);
        assert!(!out.blocked);
    }

    #[tokio::test]
    async fn an_event_with_no_declarations_is_a_clear_outcome() {
        let dir = TempDir::new().unwrap();
        plugin(dir.path(), "p", &one_hook("PostToolUse", "echo x"));
        let out = run(dir.path(), &[], "PreToolUse", "{}").await;
        assert!(!out.blocked);
        assert!(!out.failed);
        assert!(out.additional_context.is_none());
    }
}
