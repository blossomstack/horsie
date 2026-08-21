//! `fork_conversation`: `/fork` and `/summary-n-fork` addressed to the model
//! rather than typed into the composer, so a conversation can branch itself.
//!
//! Layered onto conversations only — the main agent and forks — because only a
//! conversation has a branch to take, and never onto an unattended session,
//! whose fork would be a second conversation with nobody in it.
//!
//! Routes through the session's mailbox exactly as the composer's `/fork` does,
//! so both entry points meet the same guards and write the same event. Nothing
//! here knows how a fork is seeded.

use crate::sessions::addressing::SessionRef;
use crate::sessions::run_forest::ForkMode;
use crate::sessions::session_actor::{ForkCommand, SessionCommand};
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Name of the built-in conversation-forking tool.
pub const FORK_CONVERSATION_TOOL: &str = "fork_conversation";

fn fork_conversation_spec() -> ToolSpec {
    ToolSpec {
        name: FORK_CONVERSATION_TOOL.to_string(),
        description: "Branch this conversation into a second one, carrying its history, and \
            give the new conversation its first instruction. Use it when the work splits into \
            a direction the user will want to steer on its own, or when this conversation has \
            grown long and the next stretch should start from a summary of it. \
            \n\nA fork is not a subagent: it shares this session's workspace but has its own \
            history, talks to the user directly, and never reports back to you. Nothing is \
            delivered to you when it finishes — so fork to hand work off, and spawn_agent to \
            get an answer back. Returns the new conversation's id once it exists."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["message"],
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The first message of the new conversation — what it \
                        should do next. It reads this after the history it was given."
                },
                "mode": {
                    "type": "string",
                    "enum": ["copy", "summary"],
                    "description": "How the new conversation starts. 'copy' (the default) \
                        carries this conversation's history verbatim. 'summary' carries a \
                        summary of it instead, costing one extra turn here and starting the \
                        fork with a far smaller context."
                }
            }
        }),
    }
}

/// Wraps a conversation's toolbox, adding `fork_conversation`.
pub struct ForkToolbox {
    inner: Arc<dyn Toolbox>,
    session: SessionRef,
    /// The conversation that branches — the main agent's id is the session's.
    caller: Uuid,
}

impl ForkToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, session: SessionRef, caller: Uuid) -> Self {
        Self {
            inner,
            session,
            caller,
        }
    }

    /// Which seeding the caller asked for. Rejected here rather than at the
    /// session, because this is the layer that advertised the two spellings.
    fn resolve_mode(input: &Value) -> Result<ForkMode, ToolCallError> {
        match input.get("mode").and_then(Value::as_str).map(str::trim) {
            None | Some("") | Some("copy") => Ok(ForkMode::Copy),
            Some("summary") => Ok(ForkMode::Summary),
            Some(other) => Err(ToolCallError::InvalidInput(format!(
                "no fork mode '{other}'; modes are copy, summary"
            ))),
        }
    }
}

#[async_trait]
impl Toolbox for ForkToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(fork_conversation_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name != FORK_CONVERSATION_TOOL {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let message = input
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'message'".to_string()))?;
        let mode = Self::resolve_mode(&input)?;
        let id = self
            .session
            .ask(|reply| {
                SessionCommand::Fork(ForkCommand::Create {
                    parent: self.caller,
                    mode,
                    message: message.to_string(),
                    reply,
                })
            })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
            .map_err(ToolCallError::ExecutionFailed)?;
        Ok(ToolOutcome::Result(Value::String(format!(
            "Forked into a new conversation: {id}. It carries on with the user from here; \
             you will hear nothing back from it."
        ))))
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
    use crate::sessions::addressing::SessionInbox;
    use horsie_actor::{
        ActorContext, CommandEffect, EventSourcedActor, InMemoryJournal, PersistenceId,
    };
    use horsie_agentcore::EmptyToolbox;
    use serde::{Deserialize, Serialize};
    use std::sync::Mutex;

    #[derive(Serialize, Deserialize, Default)]
    struct Empty;

    /// Answers `Create` the way the session will, and records what it was
    /// asked for — the tool's whole job is to turn a call into that command.
    struct StubSession {
        result: Result<Uuid, String>,
        seen: Arc<Mutex<Vec<(Uuid, ForkMode, String)>>>,
    }

    #[async_trait::async_trait]
    impl EventSourcedActor for StubSession {
        type Command = SessionInbox;
        type Event = ();
        type State = Empty;

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("fork-tool-test", "stub")
        }
        fn initial_state() -> Empty {
            Empty
        }
        fn apply_event(state: Empty, (): ()) -> Empty {
            state
        }
        async fn handle_command(
            &mut self,
            _state: &Empty,
            cmd: SessionInbox,
            _ctx: &mut ActorContext<SessionInbox>,
        ) -> CommandEffect<()> {
            if let SessionCommand::Fork(ForkCommand::Create {
                parent,
                mode,
                message,
                reply,
            }) = cmd.cmd
            {
                self.seen.lock().unwrap().push((parent, mode, message));
                let _ = reply.send(self.result.clone());
            }
            CommandEffect::none()
        }
    }

    struct Harness {
        toolbox: ForkToolbox,
        caller: Uuid,
        seen: Arc<Mutex<Vec<(Uuid, ForkMode, String)>>>,
    }

    fn harness(result: Result<Uuid, String>) -> Harness {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let session = SessionRef::new(
            crate::testing::spawn_detached(
                &horsie_actor::ActorSystem::new(Arc::new(InMemoryJournal::new())),
                StubSession {
                    result,
                    seen: Arc::clone(&seen),
                },
            ),
            crate::auth::UserId::bootstrap(),
            Uuid::new_v4(),
            None,
        );
        let caller = Uuid::new_v4();
        Harness {
            toolbox: ForkToolbox::new(Arc::new(EmptyToolbox), session, caller),
            caller,
            seen,
        }
    }

    #[tokio::test]
    async fn specs_advertise_the_tool() {
        let names: Vec<String> = harness(Ok(Uuid::new_v4()))
            .toolbox
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec![FORK_CONVERSATION_TOOL.to_string()]);
    }

    /// The one thing a model must not take from this tool is that a fork
    /// reports back the way a subagent does.
    #[test]
    fn the_description_says_nothing_comes_back() {
        let spec = fork_conversation_spec();
        assert!(spec.description.contains("never reports back"), "{spec:?}");
        assert!(spec.description.contains("spawn_agent"), "{spec:?}");
    }

    /// A fork is attributed to the conversation that asked for it, not to the
    /// session — which is what makes forking a fork nest.
    #[tokio::test]
    async fn a_call_forks_the_calling_conversation() {
        let h = harness(Ok(Uuid::new_v4()));
        let out = h
            .toolbox
            .execute(
                FORK_CONVERSATION_TOOL,
                json!({"message": "try the other migration"}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(
            *h.seen.lock().unwrap(),
            vec![(
                h.caller,
                ForkMode::Copy,
                "try the other migration".to_string()
            )]
        );
        match out {
            ToolOutcome::Result(Value::String(text)) => {
                assert!(text.contains("hear nothing back"), "{text}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn summary_mode_asks_for_a_summary_seed() {
        let h = harness(Ok(Uuid::new_v4()));
        h.toolbox
            .execute(
                FORK_CONVERSATION_TOOL,
                json!({"message": "write it up", "mode": "summary"}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(h.seen.lock().unwrap()[0].1, ForkMode::Summary);
    }

    /// Rejected here, where the two spellings were advertised, so the error
    /// can name them — and without troubling the session at all.
    #[tokio::test]
    async fn an_unknown_mode_is_rejected_without_reaching_the_session() {
        let h = harness(Ok(Uuid::new_v4()));
        let err = h
            .toolbox
            .execute(
                FORK_CONVERSATION_TOOL,
                json!({"message": "go", "mode": "branch"}),
                "tc1",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(ref m) if m.contains("copy, summary")));
        assert!(h.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_message_is_rejected() {
        let h = harness(Ok(Uuid::new_v4()));
        let err = h
            .toolbox
            .execute(FORK_CONVERSATION_TOOL, json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
        assert!(h.seen.lock().unwrap().is_empty());
    }

    /// The session's refusal — a workflow run, a subagent caller — reaches the
    /// model verbatim rather than as a generic failure.
    #[tokio::test]
    async fn the_sessions_refusal_is_what_the_model_reads() {
        let h = harness(Err("a workflow run cannot be forked".to_string()));
        let err = h
            .toolbox
            .execute(FORK_CONVERSATION_TOOL, json!({"message": "go"}), "tc1")
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolCallError::ExecutionFailed(ref m) if m == "a workflow run cannot be forked")
        );
    }

    #[tokio::test]
    async fn other_tools_pass_through() {
        let h = harness(Ok(Uuid::new_v4()));
        let err = h
            .toolbox
            .execute("read_file", json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(!matches!(err, ToolCallError::InvalidInput(ref m) if m.contains("message")));
        assert!(h.seen.lock().unwrap().is_empty());
    }
}
