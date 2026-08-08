//! Running plugin tool hooks, inline with the tool call they guard.
//!
//! Tool hooks run inline rather than over the server-initiated path for two
//! reasons: a hook wrapping a call the runtime is already handling costs no
//! extra round-trip, and an inline hook inherits the existing `CancelCall`
//! path — a user hitting Stop interrupts a slow hook for free.
//!
//! What each hook did rides back on the tool response as a `HookRecord`, so the
//! server can journal it and the user can see what a plugin changed.

use horsie_models::hooks::HookRecord;
use horsie_models::runtime::{ToolCall, ToolError, ToolResult};
use horsie_support::plugin::hooks::{
    HookEvent, HookInvocation, HookOutput, Permission, Verdict, claude_aliases,
};
use serde_json::Value;

use super::{matching, run_one};
use crate::state::RuntimeState;
use crate::workspace::WorkspaceRegistry;

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
    call_id: &str,
    call: ToolCall,
) -> (ToolResult, Vec<HookRecord>) {
    let Some(plugins_dir) = registry.plugins_dir() else {
        return (
            crate::tools::dispatch(registry, state, agent, call).await,
            Vec::new(),
        );
    };
    let name = tool_name(&call);
    let mut subjects = vec![name];
    subjects.extend_from_slice(claude_aliases(name));
    let hook_path = registry.hook_path();
    let mut records = Vec::new();

    // --- PreToolUse ---
    let mut call = call;
    for (root, plugin, decl) in matching(plugins_dir, HookEvent::PreToolUse, &subjects) {
        let input = call_input(&call);
        let invocation = HookInvocation::PreToolUse {
            tool: name,
            tool_call_id: call_id,
            input: &input,
        };
        let (out, record) = run_one(&root, &plugin, &decl, hook_path, invocation).await;
        // Pushed before the denial returns, so a denied call still carries the
        // record of what denied it.
        records.push(record);

        if denies(&out) {
            // Fail closed: a guard that could not run is not a guard. A
            // deliberate divergence from Claude Code, and it applies to
            // `PreToolUse` alone — every other event runs after the fact.
            let reason = deny_reason(&out, &plugin, name);
            return (ToolResult::Err(ToolError { reason }), records);
        }
        if let Some(updated) = &out.updated_input
            && let Some(rewritten) = with_input(&call, updated.clone())
        {
            call = rewritten;
        }
    }

    let mut result = crate::tools::dispatch(registry, state, agent, call.clone()).await;

    // --- PostToolUse ---
    for (root, plugin, decl) in matching(plugins_dir, HookEvent::PostToolUse, &subjects) {
        let (response, is_error) = match &result {
            ToolResult::Ok(o) => (o.stdout.clone(), false),
            ToolResult::Err(e) => (e.reason.clone(), true),
        };
        let input = call_input(&call);
        let invocation = HookInvocation::PostToolUse {
            tool: name,
            tool_call_id: call_id,
            input: &input,
            response: &response,
            is_error,
        };
        let (out, record) = run_one(&root, &plugin, &decl, hook_path, invocation).await;

        // The tool already ran: a failure here is recorded but never fatal, and
        // never rewrites a result the hook could not read.
        if !matches!(out.verdict, Verdict::Failed { .. })
            && let ToolResult::Ok(o) = &mut result
        {
            if let Some(replacement) = &out.updated_tool_output {
                o.stdout = replacement.clone();
            }
            if let Some(ctx) = &out.additional_context {
                o.stdout = format!("{}\n\n{ctx}", o.stdout);
            }
        }
        records.push(record);
    }

    (result, records)
}

/// Whether a `PreToolUse` outcome stops the call.
///
/// A failure denies as surely as a refusal does, but they are different facts —
/// which is why the record keeps them in different arms and only this predicate
/// collapses them.
fn denies(out: &HookOutput) -> bool {
    matches!(out.verdict, Verdict::Block { .. } | Verdict::Failed { .. })
        || matches!(out.permission, Some(Permission::Deny { .. }))
}

/// Why a `PreToolUse` outcome denied the call, phrased for the model.
fn deny_reason(out: &HookOutput, plugin: &str, tool: &str) -> String {
    let refused = match (&out.verdict, &out.permission) {
        (Verdict::Block { reason }, _) => Some(reason.clone()),
        (_, Some(Permission::Deny { reason })) => Some(reason.clone()),
        _ => None,
    };
    match refused {
        Some(Some(r)) => format!("plugin '{plugin}' blocked '{tool}': {r}"),
        Some(None) => format!("plugin '{plugin}' blocked '{tool}'"),
        None => format!("a '{plugin}' hook for '{tool}' could not be run, so the call was denied"),
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
pub(crate) mod tests {
    use super::*;
    use horsie_models::Workspace;
    use horsie_models::hooks::{HookAction, PostToolUseOutcome, PreToolUseOutcome};
    use horsie_models::runtime::BashInput;
    use std::path::Path;
    use tempfile::TempDir;

    /// A plugin declaring one hook whose command is a shell snippet.
    pub(crate) fn plugin(plugins: &Path, name: &str, event: &str, matcher: &str, command: &str) {
        let dir = plugins.join(name);
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        let m = if matcher.is_empty() {
            String::new()
        } else {
            format!(r#""matcher":"{matcher}","#)
        };
        std::fs::write(
            dir.join("hooks/hooks.json"),
            format!(
                r#"{{"hooks":{{"{event}":[{{{m}"hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#
            ),
        )
        .unwrap();
    }

    pub(crate) struct Env {
        _work: TempDir,
        _plugins: TempDir,
        pub(crate) registry: WorkspaceRegistry,
        state: RuntimeState,
    }

    pub(crate) fn env(plugins: TempDir) -> Env {
        let work = TempDir::new().unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "main".into(),
            path: work.path().to_path_buf(),
        }])
        .with_plugins(Some(plugins.path().to_path_buf()), Vec::new());
        Env {
            _work: work,
            _plugins: plugins,
            registry,
            state: RuntimeState::new(),
        }
    }

    fn echo() -> ToolCall {
        ToolCall::Bash(BashInput {
            command: "echo hello".to_string(),
            timeout_secs: None,
        })
    }

    async fn run(e: &Env, call: ToolCall) -> (ToolResult, Vec<HookRecord>) {
        dispatch_with_hooks(&e.registry, &e.state, "agent-1", "call-1", call).await
    }

    #[tokio::test]
    async fn with_no_plugin_library_nothing_is_recorded() {
        let work = TempDir::new().unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "main".into(),
            path: work.path().to_path_buf(),
        }]);
        let (result, hooks) =
            dispatch_with_hooks(&registry, &RuntimeState::new(), "a", "call-1", echo()).await;
        assert!(matches!(result, ToolResult::Ok(_)));
        assert!(hooks.is_empty());
    }

    /// A matcher that does not select the tool must not run the hook at all.
    #[tokio::test]
    async fn a_non_matching_hook_neither_runs_nor_records() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "p", "PreToolUse", "Write", "exit 2");
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        assert!(matches!(result, ToolResult::Ok(_)), "bash is not Write");
        assert!(hooks.is_empty());
    }

    /// The Claude alias table in action: a `Bash` matcher selects horsie's
    /// snake_case `bash`.
    #[tokio::test]
    async fn a_claude_named_matcher_selects_the_horsie_tool() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "p",
            "PreToolUse",
            "Bash",
            "echo denied 1>&2; exit 2",
        );
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        match result {
            ToolResult::Err(ToolError { reason }) => {
                assert!(reason.contains("denied"), "{reason}");
                assert!(reason.contains("'p'"), "the plugin must be named: {reason}");
            }
            ToolResult::Ok(o) => panic!("expected a denial, got {o:?}"),
        }
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].plugin, "p");
        match &hooks[0].action {
            HookAction::PreToolUse(r) => {
                assert_eq!(r.call.tool, "bash");
                assert_eq!(r.call.tool_call_id, "call-1");
                assert!(matches!(r.outcome, PreToolUseOutcome::Denied(_)));
            }
            other => panic!("expected a PreToolUse action, got {other:?}"),
        }
    }

    /// Fail closed, and recorded as an outage rather than a decision — they are
    /// different arms now, so a record cannot claim a hook decided anything.
    #[tokio::test]
    async fn a_failing_pre_hook_denies_and_is_recorded_as_failed() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "p", "PreToolUse", "", "exit 1");
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        match result {
            ToolResult::Err(ToolError { reason }) => {
                assert!(reason.contains("could not be run"), "{reason}");
            }
            ToolResult::Ok(o) => panic!("a guard that could not run must deny, got {o:?}"),
        }
        match &hooks[0].action {
            HookAction::PreToolUse(r) => {
                assert!(matches!(r.outcome, PreToolUseOutcome::Failed(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rewritten_input_changes_what_runs_and_is_recorded_as_a_diff() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "p",
            "PreToolUse",
            "",
            r#"printf '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"echo rewritten\"}}}'"#,
        );
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        match result {
            ToolResult::Ok(o) => assert!(
                o.stdout.contains("rewritten"),
                "the rewritten command must be what ran: {}",
                o.stdout
            ),
            ToolResult::Err(e) => panic!("expected success, got {e:?}"),
        }
        match &hooks[0].action {
            HookAction::PreToolUse(r) => match &r.outcome {
                PreToolUseOutcome::Allowed(a) => {
                    let rewrite = a.input.as_ref().expect("a rewrite");
                    assert!(rewrite.before.contains("echo hello"));
                    assert!(rewrite.after.contains("echo rewritten"));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// A hook must not be able to corrupt a call into something unrunnable.
    #[tokio::test]
    async fn an_undeserializable_rewrite_is_ignored() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "p",
            "PreToolUse",
            "",
            r#"printf '{\"hookSpecificOutput\":{\"updatedInput\":{\"nonsense\":true}}}'"#,
        );
        let e = env(plugins);
        let (result, _) = run(&e, echo()).await;
        match result {
            ToolResult::Ok(o) => assert!(o.stdout.contains("hello"), "original must still run"),
            ToolResult::Err(e) => panic!("expected success, got {e:?}"),
        }
    }

    /// The bug this reshape closes: `PreToolUse` has no `additionalContext`, so
    /// a hook setting it changes nothing and the record does not pretend
    /// otherwise.
    #[tokio::test]
    async fn additional_context_on_pre_tool_use_is_ignored() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "p",
            "PreToolUse",
            "",
            r#"printf '{\"hookSpecificOutput\":{\"additionalContext\":\"nope\"}}'"#,
        );
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        match result {
            ToolResult::Ok(o) => assert!(
                !o.stdout.contains("nope"),
                "context must not leak into the result: {}",
                o.stdout
            ),
            ToolResult::Err(e) => panic!("expected success, got {e:?}"),
        }
        match &hooks[0].action {
            HookAction::PreToolUse(r) => {
                assert!(matches!(r.outcome, PreToolUseOutcome::Allowed(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn post_hook_output_rewrite_is_applied_and_recorded() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "p",
            "PostToolUse",
            "",
            r#"printf '{\"hookSpecificOutput\":{\"updatedToolOutput\":\"replaced\"}}'"#,
        );
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "replaced"),
            ToolResult::Err(e) => panic!("expected success, got {e:?}"),
        }
        match &hooks[0].action {
            HookAction::PostToolUse(r) => match &r.outcome {
                PostToolUseOutcome::Ran(ran) => {
                    let rewrite = ran.output.as_ref().expect("a rewrite");
                    assert!(rewrite.before.contains("hello"));
                    assert_eq!(rewrite.after, "replaced");
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// A hook that changes nothing is still recorded: "a guard ran and allowed
    /// this" is part of the audit trail. And it cannot record a rewrite it did
    /// not make — the halves are one value.
    #[tokio::test]
    async fn a_no_op_hook_is_still_recorded_with_nothing_rewritten() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "p", "PostToolUse", "", "true");
        let e = env(plugins);
        let (_, hooks) = run(&e, echo()).await;
        assert_eq!(hooks.len(), 1);
        match &hooks[0].action {
            HookAction::PostToolUse(r) => match &r.outcome {
                PostToolUseOutcome::Ran(ran) => {
                    assert!(ran.output.is_none() && ran.additional_context.is_none());
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// PostToolUse runs after the fact, so its failure must not damage a result
    /// that already exists.
    #[tokio::test]
    async fn a_failing_post_hook_leaves_the_result_intact() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "p", "PostToolUse", "", "exit 1");
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        match result {
            ToolResult::Ok(o) => assert!(o.stdout.contains("hello")),
            ToolResult::Err(e) => panic!("a post-hook failure must not fail the call, got {e:?}"),
        }
        match &hooks[0].action {
            HookAction::PostToolUse(r) => {
                assert!(matches!(r.outcome, PostToolUseOutcome::Failed(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn several_hooks_run_in_plugin_order_and_each_is_recorded() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "a-first", "PostToolUse", "", "true");
        plugin(plugins.path(), "b-second", "PostToolUse", "", "true");
        let e = env(plugins);
        let (_, hooks) = run(&e, echo()).await;
        assert_eq!(
            hooks.iter().map(|h| h.plugin.as_str()).collect::<Vec<_>>(),
            vec!["a-first", "b-second"]
        );
    }

    /// The first denial stops the chain — a later hook cannot un-block it.
    #[tokio::test]
    async fn the_first_denial_stops_the_chain() {
        let plugins = TempDir::new().unwrap();
        plugin(plugins.path(), "a-blocks", "PreToolUse", "", "exit 2");
        plugin(plugins.path(), "b-never", "PreToolUse", "", "true");
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        assert!(matches!(result, ToolResult::Err(_)));
        assert_eq!(hooks.len(), 1, "the second hook must not have run");
        assert_eq!(hooks[0].plugin, "a-blocks");
    }

    /// horsie has no permission prompt, so `ask` allows — and is recorded as
    /// `Ask` rather than laundered into a plain allow.
    #[tokio::test]
    async fn permission_decision_ask_allows_and_says_so() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "p",
            "PreToolUse",
            "",
            r#"printf '{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\"}}'"#,
        );
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        assert!(matches!(result, ToolResult::Ok(_)));
        match &hooks[0].action {
            HookAction::PreToolUse(r) => assert!(matches!(r.outcome, PreToolUseOutcome::Ask)),
            other => panic!("{other:?}"),
        }
    }

    /// `systemMessage` is addressed to the user, so it reaches the record and
    /// never the model's view of the result.
    #[tokio::test]
    async fn a_system_message_is_recorded_and_never_injected() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "p",
            "PostToolUse",
            "",
            r#"printf '{\"systemMessage\":\"this repo pins node 22\"}'"#,
        );
        let e = env(plugins);
        let (result, hooks) = run(&e, echo()).await;
        match result {
            ToolResult::Ok(o) => assert!(!o.stdout.contains("node 22"), "{}", o.stdout),
            ToolResult::Err(e) => panic!("expected success, got {e:?}"),
        }
        match &hooks[0].action {
            HookAction::PostToolUse(r) => {
                assert_eq!(r.system_message.as_deref(), Some("this repo pins node 22"));
            }
            other => panic!("{other:?}"),
        }
    }
}
