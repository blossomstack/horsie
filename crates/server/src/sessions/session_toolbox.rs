//! The one wrapper for every tool the session answers.
//!
//! Four wrappers do the same three things today — advertise a [`ToolSpec`],
//! match one tool name, and send a typed `SessionCommand` awaiting a reply:
//! `SubAgentToolbox`, `AskUserToolbox`, `SessionTitleToolbox` and
//! `StepResultToolbox`. Once a tool call is routed to the runner that owns the
//! calling agent and offered around its capabilities, that dispatch is uniform,
//! so all four collapse into this: a set of specs, and a forward.
//!
//! What is left bespoke is the *schema*, which now sits on the capability that
//! answers the call. That is the last place a tool's arguments and its
//! handler's input type could drift apart.
//!
//! The wrappers this replaces are not the ones that execute against a real
//! service — the sandbox, MCP, memory and the control plane never go through
//! the mailbox, and they stay layered toolboxes.
//!
//! # Why the sink is a trait
//!
//! Forwarding is defined here against [`ToolSink`] rather than against a
//! session command, so this seam is testable — and lands — before the actor
//! that will implement it. The real sink sends the call to the session, which
//! resolves `agents[id]` to a runner and offers the call to its capabilities.

use crate::sessions::runners::action::Action;
use crate::sessions::runners::ids::AgentId;
use crate::sessions::runners::message::ToolCall;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::Value;
use std::sync::Arc;

/// Where a forwarded tool call goes.
#[async_trait]
pub trait ToolSink: Send + Sync {
    /// Hand the call to whoever owns this agent, and answer with what came
    /// back.
    ///
    /// `Err` is a refusal the model reads and can correct — a cap reached, a
    /// name that resolved to nothing — not a transport failure.
    async fn forward(&self, agent: AgentId, call: ToolCall) -> Result<String, String>;
}

/// `inner` plus the tools the session answers for this agent.
pub struct SessionToolbox {
    inner: Arc<dyn Toolbox>,
    /// Contributed by the capabilities this agent was equipped with, in the
    /// order they were folded.
    specs: Vec<ToolSpec>,
    agent: AgentId,
    sink: Arc<dyn ToolSink>,
}

impl SessionToolbox {
    #[must_use]
    pub fn new(
        inner: Arc<dyn Toolbox>,
        specs: Vec<ToolSpec>,
        agent: AgentId,
        sink: Arc<dyn ToolSink>,
    ) -> Self {
        Self {
            inner,
            specs,
            agent,
            sink,
        }
    }

    fn owns(&self, name: &str) -> bool {
        self.specs.iter().any(|s| s.name == name)
    }
}

#[async_trait]
impl Toolbox for SessionToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(self.specs.iter().cloned());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        // Only what this agent was equipped with. A name we did not advertise
        // belongs to a layer further in, and passing it down rather than
        // failing is what lets the sandbox and the plugin scan own a namespace
        // nobody here can enumerate.
        if !self.owns(name) {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let call = ToolCall {
            id: tool_call_id.to_string(),
            name: name.to_string(),
            input,
        };
        match self.sink.forward(self.agent, call).await {
            Ok(text) => Ok(ToolOutcome::Result(Value::String(text))),
            Err(refusal) => Err(ToolCallError::ExecutionFailed(refusal)),
        }
    }
}

/// Render what a capability decided into the one string a tool call answers
/// with.
///
/// A capability returns actions rather than text — it decides, the session
/// performs — so the reply the model sees is assembled from them here, in one
/// place, rather than by each capability formatting its own.
#[must_use]
pub fn reply_text(actions: &[Action]) -> String {
    let replies: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            Action::Reply { text } => Some(text.as_str()),
            Action::StartAgent { .. }
            | Action::CreateChild { .. }
            | Action::Deliver { .. }
            | Action::Cancel { .. } => None,
        })
        .collect();
    if !replies.is_empty() {
        return replies.join("\n");
    }
    // A call that only asked for something to be created answers with what it
    // created, so the model has a handle to ask about later.
    for action in actions {
        if let Action::CreateChild { id, .. } = action {
            return format!("started: {id}");
        }
    }
    "done".to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::runners::ids::{RunnerId, RunnerKind};
    use std::sync::Mutex;

    /// A toolbox that answers one name and records what it was asked.
    struct Inner {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Toolbox for Inner {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "bash".into(),
                description: "run a command".into(),
                input_schema: serde_json::json!({}),
            }]
        }

        async fn execute(
            &self,
            name: &str,
            _input: Value,
            _id: &str,
        ) -> Result<ToolOutcome, ToolCallError> {
            self.seen.lock().unwrap().push(name.to_string());
            Ok(ToolOutcome::Result(Value::String("inner".into())))
        }
    }

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<(AgentId, String)>>,
        refuse: Option<String>,
    }

    #[async_trait]
    impl ToolSink for Recorder {
        async fn forward(&self, agent: AgentId, call: ToolCall) -> Result<String, String> {
            self.calls.lock().unwrap().push((agent, call.name.clone()));
            match &self.refuse {
                Some(reason) => Err(reason.clone()),
                None => Ok(format!("handled {}", call.name)),
            }
        }
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
        }
    }

    fn boxed(specs: Vec<ToolSpec>, sink: Arc<Recorder>) -> (SessionToolbox, Arc<Inner>, AgentId) {
        let inner = Arc::new(Inner {
            seen: Mutex::new(Vec::new()),
        });
        let agent = AgentId::new_v4();
        (
            SessionToolbox::new(inner.clone(), specs, agent, sink),
            inner,
            agent,
        )
    }

    /// The agent id travels with the call. Without it the session cannot
    /// resolve which runner owns the caller, and routing by owner is the whole
    /// mechanism.
    #[tokio::test]
    async fn a_forwarded_call_carries_the_calling_agent() {
        let sink = Arc::new(Recorder::default());
        let (tb, inner, agent) = boxed(vec![spec("ask_user")], sink.clone());

        let out = tb
            .execute("ask_user", serde_json::json!({}), "t1")
            .await
            .unwrap();
        assert_eq!(out.expect_value(), Value::String("handled ask_user".into()));
        assert_eq!(
            sink.calls.lock().unwrap().as_slice(),
            &[(agent, "ask_user".to_string())]
        );
        assert!(
            inner.seen.lock().unwrap().is_empty(),
            "a forwarded call must not also reach the inner toolbox"
        );
    }

    /// A name this agent was not equipped with goes inward, not to the
    /// session. That is what lets the sandbox and the plugin scan own a
    /// namespace this wrapper cannot enumerate.
    #[tokio::test]
    async fn an_unowned_call_passes_inward() {
        let sink = Arc::new(Recorder::default());
        let (tb, inner, _) = boxed(vec![spec("ask_user")], sink.clone());

        tb.execute("bash", serde_json::json!({}), "t1")
            .await
            .unwrap();
        assert_eq!(inner.seen.lock().unwrap().as_slice(), &["bash".to_string()]);
        assert!(sink.calls.lock().unwrap().is_empty());
    }

    /// Both sets are advertised, so the model sees the sandbox's tools and the
    /// session's as one list.
    #[test]
    fn specs_are_the_inner_ones_plus_the_equipped_ones() {
        let sink = Arc::new(Recorder::default());
        let (tb, _, _) = boxed(vec![spec("ask_user"), spec("spawn_agent")], sink);
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["bash", "ask_user", "spawn_agent"]);
    }

    /// A refusal reaches the model as a failed call rather than as a result,
    /// so it reads as something to correct rather than as an answer.
    #[tokio::test]
    async fn a_refusal_fails_the_call() {
        let sink = Arc::new(Recorder {
            calls: Mutex::new(Vec::new()),
            refuse: Some("4 subagents already active".into()),
        });
        let (tb, _, _) = boxed(vec![spec("spawn_agent")], sink);

        let err = tb
            .execute("spawn_agent", serde_json::json!({}), "t1")
            .await
            .expect_err("a refusal is not a result");
        assert!(err.to_string().contains("already active"), "{err}");
    }

    /// A capability that only asked for a child answers with its id, so the
    /// model has a handle to ask about later.
    #[test]
    fn a_create_answers_with_the_child_id() {
        let id = RunnerId::new_v4();
        let text = reply_text(&[Action::CreateChild {
            id,
            kind: RunnerKind::SubAgent,
            args: crate::sessions::runners::action::RunnerArgs::Workflow {
                source: crate::sessions::runners::action::WorkflowSource::Named("w".into()),
                input: String::new(),
            },
            parent: AgentId::new_v4(),
        }]);
        assert!(text.contains(&id.to_string()), "{text}");
    }

    /// An explicit reply wins over the create fallback: a capability that took
    /// the trouble to word something has said what the model should read.
    #[test]
    fn an_explicit_reply_is_what_the_model_reads() {
        let text = reply_text(&[Action::Reply {
            text: "no workflow named `nope`".into(),
        }]);
        assert_eq!(text, "no workflow named `nope`");
    }
}
