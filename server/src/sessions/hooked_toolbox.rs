//! `Toolbox` decorator that runs `PreToolUse` and `PostToolUse` hooks.
//!
//! This is the whole of tool-hook support. `agentcore`'s loop dispatches every
//! tool through `Toolbox::execute`, so wrapping the box is enough and the loop
//! never learns hooks exist — the same way `FilteredToolbox` narrows a tool set
//! without the loop knowing about allowlists.
//!
//! Matchers are evaluated here, against the manifest the runtime reported at
//! session start, so a session whose plugins declare no matching hook never
//! round-trips to the runtime at all.

use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, Toolbox, ToolSpec};
use horsie_models::runtime::{HookDeclWire, HookOutcomeWire};
use horsie_runtime_client::RuntimeClient;
use horsie_support::plugin::hooks::matcher_applies;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct HookedToolbox {
    inner: Arc<dyn Toolbox>,
    client: RuntimeClient,
    /// Only the declarations for tool events; turn events are dispatched
    /// elsewhere and would only be dead weight in the per-call matcher scan.
    decls: Vec<HookDeclWire>,
}

impl HookedToolbox {
    /// Wrap `inner` when — and only when — some declaration targets a tool
    /// event. A session with no tool hooks gets its toolbox back untouched, so
    /// the decorator costs nothing rather than costing a matcher scan per call.
    pub fn wrap(
        inner: Arc<dyn Toolbox>,
        client: RuntimeClient,
        decls: Vec<HookDeclWire>,
    ) -> Arc<dyn Toolbox> {
        let decls: Vec<HookDeclWire> = decls
            .into_iter()
            .filter(|d| d.event == "PreToolUse" || d.event == "PostToolUse")
            .collect();
        if decls.is_empty() {
            return inner;
        }
        Arc::new(HookedToolbox {
            inner,
            client,
            decls,
        })
    }

    /// Whether any declaration for `event` selects `tool`.
    fn matches(&self, event: &str, tool: &str) -> bool {
        self.decls
            .iter()
            .filter(|d| d.event == event)
            .any(|d| matcher_applies(d.matcher.as_deref(), tool))
    }

    /// Run an event, treating a transport failure as a hook failure so the
    /// caller's fail-closed decision covers a dead runtime too.
    async fn run(&self, event: &str, payload: &Value) -> HookOutcomeWire {
        let body = payload.to_string();
        match self.client.run_hook(event, &body).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(event, error = %e, "hook dispatch failed");
                failed_outcome()
            }
        }
    }
}

/// The outcome for a hook that could not be reached at all.
fn failed_outcome() -> HookOutcomeWire {
    HookOutcomeWire {
        blocked: false,
        reason: None,
        additional_context: None,
        updated_input: None,
        updated_tool_output: None,
        system_message: None,
        stop: false,
        stop_reason: None,
        failed: true,
    }
}

/// Why a `PreToolUse` outcome denied the call, phrased for the model.
fn deny_reason(outcome: &HookOutcomeWire, tool: &str) -> String {
    if outcome.blocked {
        match &outcome.reason {
            Some(r) => format!("a plugin hook blocked '{tool}': {r}"),
            None => format!("a plugin hook blocked '{tool}'"),
        }
    } else {
        // Fail closed: the guard could not run, so the call it guards does not
        // happen. Said plainly so this is not mistaken for a policy decision.
        format!("a plugin hook for '{tool}' could not be run, so the call was denied")
    }
}

/// Apply a `PostToolUse` outcome to a successful tool result.
fn apply_post(value: Value, outcome: &HookOutcomeWire) -> Value {
    let mut out = match &outcome.updated_tool_output {
        Some(replacement) => Value::String(replacement.clone()),
        None => value,
    };
    if let Some(ctx) = &outcome.additional_context {
        let base = out
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| out.to_string());
        out = Value::String(format!("{base}\n\n{ctx}"));
    }
    out
}

#[async_trait]
impl Toolbox for HookedToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        let mut input = input;

        if self.matches("PreToolUse", name) {
            let payload = json!({
                "hook_event_name": "PreToolUse",
                "tool_name": name,
                "tool_input": input,
            });
            let outcome = self.run("PreToolUse", &payload).await;
            // Fail closed: a guard that could not run is not a guard. A
            // deliberate divergence from Claude Code, and it applies to
            // `PreToolUse` alone — every other event runs after the fact.
            if outcome.blocked || outcome.failed {
                return Err(ToolCallError::ExecutionFailed(deny_reason(&outcome, name)));
            }
            if let Some(updated) = outcome.updated_input.as_deref()
                && let Ok(v) = serde_json::from_str::<Value>(updated)
            {
                input = v;
            }
        }

        let result = self.inner.execute(name, input.clone()).await;

        if self.matches("PostToolUse", name) {
            let (response, is_error) = match &result {
                Ok(v) => (v.clone(), false),
                Err(e) => (Value::String(e.to_string()), true),
            };
            let payload = json!({
                "hook_event_name": "PostToolUse",
                "tool_name": name,
                "tool_input": input,
                "tool_response": response,
                "is_error": is_error,
            });
            let outcome = self.run("PostToolUse", &payload).await;
            // The tool already ran: a failure here is logged, never fatal, and
            // never rewrites a result the hook could not read.
            if !outcome.failed
                && let Ok(v) = result.as_ref()
            {
                return Ok(apply_post(v.clone(), &outcome));
            }
        }
        result
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
    use horsie_runtime_client::testkit::MockTransport;
    use std::sync::Mutex;

    /// Records whether the real toolbox was reached, and with what.
    struct Spy {
        calls: Arc<Mutex<Vec<Value>>>,
        result: Result<Value, String>,
    }

    #[async_trait]
    impl Toolbox for Spy {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "write_file".to_string(),
                description: String::new(),
                input_schema: json!({}),
            }]
        }
        async fn execute(&self, _name: &str, input: Value) -> Result<Value, ToolCallError> {
            self.calls.lock().unwrap().push(input);
            self.result
                .clone()
                .map_err(ToolCallError::ExecutionFailed)
        }
    }

    fn spy(result: Result<Value, String>) -> (Arc<Spy>, Arc<Mutex<Vec<Value>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Spy {
                calls: calls.clone(),
                result,
            }),
            calls,
        )
    }

    fn decl(event: &str, matcher: Option<&str>) -> HookDeclWire {
        HookDeclWire {
            event: event.to_string(),
            matcher: matcher.map(str::to_string),
        }
    }

    fn client_returning(outcome: HookOutcomeWire) -> RuntimeClient {
        RuntimeClient::new(MockTransport::ok("{}").with_hook_outcome(outcome), "test")
    }

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

    /// The optimisation that keeps hook-less sessions free: no declaration for
    /// a tool event means the decorator is never even constructed.
    #[test]
    fn wrap_returns_the_inner_box_when_no_declaration_targets_a_tool() {
        let (inner, _) = spy(Ok(json!("ok")));
        let wrapped = HookedToolbox::wrap(
            inner.clone(),
            client_returning(clear()),
            vec![decl("SessionStart", None), decl("Stop", None)],
        );
        assert_eq!(wrapped.specs().len(), 1);
        // Same allocation, so nothing was layered on.
        assert!(Arc::ptr_eq(
            &(inner as Arc<dyn Toolbox>),
            &wrapped
        ));
    }

    #[tokio::test]
    async fn a_non_matching_tool_reaches_the_inner_box_unhooked() {
        let (inner, calls) = spy(Ok(json!("ok")));
        let hooked = HookedToolbox::wrap(
            inner,
            client_returning(HookOutcomeWire {
                blocked: true,
                ..clear()
            }),
            vec![decl("PreToolUse", Some("Bash"))],
        );
        // `write_file` does not match a `Bash` matcher, so the blocking outcome
        // must never be consulted.
        let out = hooked.execute("write_file", json!({"a": 1})).await.unwrap();
        assert_eq!(out, json!("ok"));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_blocking_pre_tool_use_hook_prevents_the_call() {
        let (inner, calls) = spy(Ok(json!("ok")));
        let hooked = HookedToolbox::wrap(
            inner,
            client_returning(HookOutcomeWire {
                blocked: true,
                reason: Some("not allowed".to_string()),
                ..clear()
            }),
            vec![decl("PreToolUse", Some("Write"))],
        );
        let err = hooked
            .execute("write_file", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not allowed"), "{err}");
        assert!(err.contains("write_file"), "{err}");
        assert!(
            calls.lock().unwrap().is_empty(),
            "the tool must not have run"
        );
    }

    /// Fail closed — the decision this design makes differently from Claude Code.
    #[tokio::test]
    async fn a_failed_pre_tool_use_hook_also_denies() {
        let (inner, calls) = spy(Ok(json!("ok")));
        let hooked = HookedToolbox::wrap(
            inner,
            client_returning(HookOutcomeWire {
                failed: true,
                ..clear()
            }),
            vec![decl("PreToolUse", None)],
        );
        let err = hooked
            .execute("write_file", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not be run"), "{err}");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn updated_input_reaches_the_inner_box() {
        let (inner, calls) = spy(Ok(json!("ok")));
        let hooked = HookedToolbox::wrap(
            inner,
            client_returning(HookOutcomeWire {
                updated_input: Some(r#"{"path":"rewritten"}"#.to_string()),
                ..clear()
            }),
            vec![decl("PreToolUse", None)],
        );
        hooked
            .execute("write_file", json!({"path": "original"}))
            .await
            .unwrap();
        assert_eq!(calls.lock().unwrap()[0], json!({"path": "rewritten"}));
    }

    #[tokio::test]
    async fn updated_tool_output_replaces_what_the_model_sees() {
        let (inner, _) = spy(Ok(json!("real output")));
        let hooked = HookedToolbox::wrap(
            inner,
            client_returning(HookOutcomeWire {
                updated_tool_output: Some("replaced".to_string()),
                ..clear()
            }),
            vec![decl("PostToolUse", None)],
        );
        let out = hooked.execute("write_file", json!({})).await.unwrap();
        assert_eq!(out, json!("replaced"));
    }

    #[tokio::test]
    async fn post_tool_use_context_is_appended_to_the_output() {
        let (inner, _) = spy(Ok(json!("real output")));
        let hooked = HookedToolbox::wrap(
            inner,
            client_returning(HookOutcomeWire {
                additional_context: Some("a note".to_string()),
                ..clear()
            }),
            vec![decl("PostToolUse", None)],
        );
        let out = hooked.execute("write_file", json!({})).await.unwrap();
        assert_eq!(out, json!("real output\n\na note"));
    }

    /// PostToolUse runs after the fact, so its failure must not damage a result
    /// that already exists.
    #[tokio::test]
    async fn a_failed_post_tool_use_hook_leaves_the_result_intact() {
        let (inner, _) = spy(Ok(json!("real output")));
        let hooked = HookedToolbox::wrap(
            inner,
            client_returning(HookOutcomeWire {
                failed: true,
                updated_tool_output: Some("must not apply".to_string()),
                ..clear()
            }),
            vec![decl("PostToolUse", None)],
        );
        let out = hooked.execute("write_file", json!({})).await.unwrap();
        assert_eq!(out, json!("real output"));
    }

    #[tokio::test]
    async fn a_failing_tool_still_reaches_post_tool_use_and_keeps_its_error() {
        let (inner, _) = spy(Err("boom".to_string()));
        let hooked = HookedToolbox::wrap(
            inner,
            client_returning(clear()),
            vec![decl("PostToolUse", None)],
        );
        let err = hooked
            .execute("write_file", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "{err}");
    }
}
