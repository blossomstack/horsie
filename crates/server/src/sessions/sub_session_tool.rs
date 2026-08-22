//! `spawn_subsession`: the tool one session hands work to another with.
//!
//! A sub session is a session in its own right — it talks to the user, names
//! itself, and never reports back — so the tool that starts one is named for
//! the thing it makes. It pairs with `spawn_agent`: same shape, one
//! self-contained brief, differing only in where the result goes.
//!
//! Nothing is copied or summarised because the agent calling it already holds
//! the context and can say what matters in a sentence, exactly as it does when
//! spawning a subagent. A copy would hand the sub session the transcript of
//! work it is not doing, and a summary would spend a turn producing one.
//!
//! Layered onto sessions only — the main agent and its sub sessions — because
//! only a session has somewhere to hand work *to*, and never onto an unattended
//! session, whose sub session would have nobody in it.
//!
//! Routes through the session's mailbox exactly as the composer's `/fork` does,
//! so both entry points meet the same guards and write the same event. Nothing
//! here knows how a sub session is seeded.

use crate::sessions::addressing::SessionRef;
use crate::sessions::run_forest::SeedMode;
use crate::sessions::session_actor::{SessionCommand, SubSessionCommand};
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Name of the built-in sub-session hand-off tool.
pub const SPAWN_SUBSESSION_TOOL: &str = "spawn_subsession";

fn spawn_subsession_spec() -> ToolSpec {
    ToolSpec {
        name: SPAWN_SUBSESSION_TOOL.to_string(),
        description: "Start a sub session — a second session under this one — and hand it \
            a task. The tool form of the `/fork` a person types. Use it when the work splits \
            into a direction the user will want to steer on its own, or when what comes next \
            has little to do with what this session has been about. \
            \n\nThe sub session shares this session's workspace — the same checkout, the \
            same edits — but starts with none of this session's history, so write the task \
            the way you would write one for a subagent: self-contained, including whatever \
            it needs to know. \
            \n\nIt is not a subagent, though. It talks to the user directly and never \
            reports back to you: nothing is delivered here when it finishes. Use this to \
            hand work off, spawn_agent to get an answer back. Returns the sub session's id."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["task"],
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The complete, self-contained task for the sub \
                        session — what it should do, and everything it needs to know to \
                        start. It is the first thing the sub session reads."
                }
            }
        }),
    }
}

/// Wraps a session's toolbox, adding `spawn_subsession`.
pub struct SubSessionToolbox {
    inner: Arc<dyn Toolbox>,
    session: SessionRef,
    /// The session that branches — the main agent's id is the session's.
    caller: Uuid,
}

impl SubSessionToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, session: SessionRef, caller: Uuid) -> Self {
        Self {
            inner,
            session,
            caller,
        }
    }
}

#[async_trait]
impl Toolbox for SubSessionToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(spawn_subsession_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name != SPAWN_SUBSESSION_TOOL {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let task = input
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'task'".to_string()))?;
        let id = self
            .session
            .ask(|reply| {
                SessionCommand::SubSession(SubSessionCommand::Create {
                    parent: self.caller,
                    seed: SeedMode::Fresh,
                    message: task.to_string(),
                    reply,
                })
            })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
            .map_err(ToolCallError::ExecutionFailed)?;
        Ok(ToolOutcome::Result(Value::String(format!(
            "Started a sub session: {id}. It carries on with the user from here; you will \
             hear nothing back from it."
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
        seen: Arc<Mutex<Vec<(Uuid, SeedMode, String)>>>,
    }

    #[async_trait::async_trait]
    impl EventSourcedActor for StubSession {
        type Command = SessionInbox;
        type Event = ();
        type State = Empty;

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("sub_session-tool-test", "stub")
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
            if let SessionCommand::SubSession(SubSessionCommand::Create {
                parent,
                seed,
                message,
                reply,
            }) = cmd.cmd
            {
                self.seen.lock().unwrap().push((parent, seed, message));
                let _ = reply.send(self.result.clone());
            }
            CommandEffect::none()
        }
    }

    struct Harness {
        toolbox: SubSessionToolbox,
        caller: Uuid,
        seen: Arc<Mutex<Vec<(Uuid, SeedMode, String)>>>,
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
            toolbox: SubSessionToolbox::new(Arc::new(EmptyToolbox), session, caller),
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
        assert_eq!(names, vec![SPAWN_SUBSESSION_TOOL.to_string()]);
    }

    /// Two things a model must not take from this tool: that a sub session
    /// reports back the way a subagent does, and that it can see what was said
    /// here.
    #[test]
    fn the_description_says_nothing_comes_back_and_nothing_goes_with_it() {
        let spec = spawn_subsession_spec();
        assert!(spec.description.contains("never reports back"), "{spec:?}");
        assert!(spec.description.contains("spawn_agent"), "{spec:?}");
        assert!(
            spec.description.contains("none of this session's history"),
            "{spec:?}"
        );
        assert!(spec.description.contains("self-contained"), "{spec:?}");
        // The name says "subsession", not "fork", so the description is the
        // only place a model learns this is what `/fork` asks for.
        assert!(spec.description.contains("`/fork`"), "{spec:?}");
    }

    /// A sub session is attributed to the session that asked for it, not to the
    /// root — which is what makes sub sessions nest. And it is always `Fresh`:
    /// the tool has no way to ask for a copy, because the brief it carries is
    /// the point.
    #[tokio::test]
    async fn a_call_hands_a_brief_to_a_sub_session_of_the_caller() {
        let h = harness(Ok(Uuid::new_v4()));
        let out = h
            .toolbox
            .execute(
                SPAWN_SUBSESSION_TOOL,
                json!({"task": "try the other migration"}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(
            *h.seen.lock().unwrap(),
            vec![(
                h.caller,
                SeedMode::Fresh,
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
            .execute(SPAWN_SUBSESSION_TOOL, json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
        assert!(h.seen.lock().unwrap().is_empty());
    }

    /// The session's refusal — a workflow run, a subagent caller — reaches the
    /// model verbatim rather than as a generic failure.
    #[tokio::test]
    async fn the_sessions_refusal_is_what_the_model_reads() {
        let h = harness(Err("a workflow run cannot be branched".to_string()));
        let err = h
            .toolbox
            .execute(SPAWN_SUBSESSION_TOOL, json!({"task": "go"}), "tc1")
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolCallError::ExecutionFailed(ref m) if m == "a workflow run cannot be branched")
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
