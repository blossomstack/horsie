//! `spawn_conversation`: the tool one conversation hands work to another with.
//!
//! Named for what it does, not for what it makes. The thing it creates is a
//! *fork* everywhere else — `ForkMode`, the rail, the docs, the `/fork` a
//! person types — but the model-facing name has to survive being read on its
//! own, and "fork" promises a history this carries none of. It pairs with
//! `spawn_agent` instead: same shape, one self-contained brief, differing only
//! in where the result goes.
//!
//! Nothing is copied or summarised because the agent calling it already holds
//! the context and can say what matters in a sentence, exactly as it does when
//! spawning a subagent. A copy would hand the new conversation the transcript
//! of work it is not doing, and a summary would spend a turn producing one.
//!
//! Layered onto conversations only — the main agent and forks — because only a
//! conversation has somewhere to hand work *to*, and never onto an unattended
//! session, whose fork would be a conversation with nobody in it.
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

/// Name of the built-in conversation hand-off tool.
pub const SPAWN_CONVERSATION_TOOL: &str = "spawn_conversation";

fn spawn_conversation_spec() -> ToolSpec {
    ToolSpec {
        name: SPAWN_CONVERSATION_TOOL.to_string(),
        description: "Start a second conversation in this session and hand it a task \
            — the tool form of the `/fork` a person types. Use it when the work splits into \
            a direction the user will want to steer on its own, or when what comes next has \
            little to do with what this conversation has been about. \
            \n\nThe new conversation shares this session's workspace — the same checkout, the \
            same edits — but starts with none of this conversation's history, so write the \
            task the way you would write one for a subagent: self-contained, including \
            whatever it needs to know. \
            \n\nIt is not a subagent, though. It talks to the user directly and never reports \
            back to you: nothing is delivered here when it finishes. Use this to hand work \
            off, spawn_agent to get an answer back. Returns the new conversation's id."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["task"],
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The complete, self-contained task for the new \
                        conversation — what it should do, and everything it needs to know \
                        to start. It is the first thing the new conversation reads."
                }
            }
        }),
    }
}

/// Wraps a conversation's toolbox, adding `spawn_conversation`.
pub struct ConversationToolbox {
    inner: Arc<dyn Toolbox>,
    session: SessionRef,
    /// The conversation that branches — the main agent's id is the session's.
    caller: Uuid,
}

impl ConversationToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, session: SessionRef, caller: Uuid) -> Self {
        Self {
            inner,
            session,
            caller,
        }
    }
}

#[async_trait]
impl Toolbox for ConversationToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(spawn_conversation_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name != SPAWN_CONVERSATION_TOOL {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let task = input
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'task'".to_string()))?;
        let id = self
            .session
            .ask(|reply| {
                SessionCommand::Fork(ForkCommand::Create {
                    parent: self.caller,
                    mode: ForkMode::Fresh,
                    message: task.to_string(),
                    reply,
                })
            })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
            .map_err(ToolCallError::ExecutionFailed)?;
        Ok(ToolOutcome::Result(Value::String(format!(
            "Started a new conversation: {id}. It carries on with the user from here; you \
             will hear nothing back from it."
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
        toolbox: ConversationToolbox,
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
            crate::projects::ProjectId::generate(),
            Uuid::new_v4(),
            None,
        );
        let caller = Uuid::new_v4();
        Harness {
            toolbox: ConversationToolbox::new(Arc::new(EmptyToolbox), session, caller),
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
        assert_eq!(names, vec![SPAWN_CONVERSATION_TOOL.to_string()]);
    }

    /// Two things a model must not take from this tool: that a fork reports
    /// back the way a subagent does, and that it can see what was said here.
    #[test]
    fn the_description_says_nothing_comes_back_and_nothing_goes_with_it() {
        let spec = spawn_conversation_spec();
        assert!(spec.description.contains("never reports back"), "{spec:?}");
        assert!(spec.description.contains("spawn_agent"), "{spec:?}");
        assert!(
            spec.description
                .contains("none of this conversation's history"),
            "{spec:?}"
        );
        assert!(spec.description.contains("self-contained"), "{spec:?}");
        // The name no longer says "fork", so the description is the only place
        // a model learns this is what a user typing `/fork` is asking for.
        assert!(spec.description.contains("`/fork`"), "{spec:?}");
    }

    /// A fork is attributed to the conversation that asked for it, not to the
    /// session — which is what makes forking a fork nest. And it is always
    /// `Fresh`: the tool has no way to ask for a copy, because the brief it
    /// carries is the point.
    #[tokio::test]
    async fn a_call_hands_a_brief_to_a_fork_of_the_calling_conversation() {
        let h = harness(Ok(Uuid::new_v4()));
        let out = h
            .toolbox
            .execute(
                SPAWN_CONVERSATION_TOOL,
                json!({"task": "try the other migration"}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(
            *h.seen.lock().unwrap(),
            vec![(
                h.caller,
                ForkMode::Fresh,
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
    async fn a_missing_task_is_rejected() {
        let h = harness(Ok(Uuid::new_v4()));
        let err = h
            .toolbox
            .execute(SPAWN_CONVERSATION_TOOL, json!({}), "tc1")
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
            .execute(SPAWN_CONVERSATION_TOOL, json!({"task": "go"}), "tc1")
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
        assert!(!matches!(err, ToolCallError::InvalidInput(ref m) if m.contains("task")));
        assert!(h.seen.lock().unwrap().is_empty());
    }
}
