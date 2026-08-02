//! Server-owned tools for delegating work: `spawn_agent` starts a subagent
//! and `subagent_status` inspects the caller's subtree. Both route through the
//! owning session's mailbox — the session is the one place that enforces
//! limits, persists the tree, and owns the child actors.
//!
//! Layered onto every agent in a session, main and sub alike (which is what
//! makes sub-spawning work), carrying the *calling* agent's identity so
//! spawns are attributed to the right parent.

use crate::sessions::session_actor::SessionCommand;
use crate::sessions::subagents::SubAgentParent;
use async_trait::async_trait;
use horsie_actor::ActorRef;
use horsie_agentcore::{ToolCallError, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Name of the built-in subagent-spawning tool.
pub const SPAWN_AGENT_TOOL: &str = "spawn_agent";
/// Name of the built-in subagent-inspection tool.
pub const SUBAGENT_STATUS_TOOL: &str = "subagent_status";

fn spawn_agent_spec() -> ToolSpec {
    ToolSpec {
        name: SPAWN_AGENT_TOOL.to_string(),
        description: "Spawn a subagent to work on a task independently and in parallel. \
            Returns immediately with the subagent's id; when it finishes, its result is \
            delivered back to you as a message. Use subagent_status to check progress. \
            Spawning fails when the session's subagent limits (depth or concurrency) are \
            reached."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["label", "task"],
            "properties": {
                "label": {
                    "type": "string",
                    "description": "A short human-readable label for the subagent (a few words)."
                },
                "task": {
                    "type": "string",
                    "description": "The complete, self-contained task for the subagent. It \
                        inherits your model and tools but not your conversation — include \
                        everything it needs to know."
                }
            }
        }),
    }
}

fn subagent_status_spec() -> ToolSpec {
    ToolSpec {
        name: SUBAGENT_STATUS_TOOL.to_string(),
        description: "Check on subagents. With `id`, returns that subagent's status, and \
            its output or error once finished. Without `id`, lists your whole subagent \
            subtree with statuses."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "A subagent id returned by spawn_agent. Omit to list your subtree."
                }
            }
        }),
    }
}

/// Wraps an agent's toolbox, adding `spawn_agent` and `subagent_status`.
pub struct SubAgentToolbox {
    inner: Arc<dyn Toolbox>,
    session: ActorRef<SessionCommand>,
    /// Which agent this toolbox belongs to — the parent spawns attribute to.
    caller: SubAgentParent,
}

impl SubAgentToolbox {
    pub fn new(
        inner: Arc<dyn Toolbox>,
        session: ActorRef<SessionCommand>,
        caller: SubAgentParent,
    ) -> Self {
        Self {
            inner,
            session,
            caller,
        }
    }
}

#[async_trait]
impl Toolbox for SubAgentToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(spawn_agent_spec());
        specs.push(subagent_status_spec());
        specs
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        if name == SPAWN_AGENT_TOOL {
            let label = input
                .get("label")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolCallError::InvalidInput("missing 'label'".to_string()))?;
            let task = input
                .get("task")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolCallError::InvalidInput("missing 'task'".to_string()))?;
            let id = self
                .session
                .ask(|reply| SessionCommand::SpawnSubAgent {
                    caller: self.caller,
                    label: label.to_string(),
                    task: task.to_string(),
                    reply,
                })
                .await
                .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
                .map_err(ToolCallError::ExecutionFailed)?;
            return Ok(Value::String(format!("Subagent spawned: {id}")));
        }
        if name == SUBAGENT_STATUS_TOOL {
            let id = input
                .get("id")
                .and_then(Value::as_str)
                .map(|s| {
                    Uuid::parse_str(s).map_err(|_| {
                        ToolCallError::InvalidInput(format!("'{s}' is not a subagent id"))
                    })
                })
                .transpose()?;
            let rendered = self
                .session
                .ask(|reply| SessionCommand::SubAgentStatus {
                    caller: self.caller,
                    id,
                    reply,
                })
                .await
                .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
                .map_err(ToolCallError::ExecutionFailed)?;
            return Ok(Value::String(rendered));
        }
        self.inner.execute(name, input).await
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
    use horsie_actor::{
        ActorContext, CommandEffect, EventSourcedActor, InMemoryJournal, PersistenceId, spawn_root,
    };
    use horsie_agentcore::EmptyToolbox;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Default)]
    struct Empty;

    /// Answers spawn/status asks the way the session will, so tool behavior
    /// is tested without a session actor.
    struct StubSession {
        spawn_result: Result<Uuid, String>,
    }

    #[async_trait::async_trait]
    impl EventSourcedActor for StubSession {
        type Command = SessionCommand;
        type Event = ();
        type State = Empty;

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("spawn-tool-test", "stub")
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
            cmd: SessionCommand,
            _ctx: &mut ActorContext<Self>,
        ) -> CommandEffect<()> {
            match cmd {
                SessionCommand::SpawnSubAgent { reply, .. } => {
                    let _ = reply.send(self.spawn_result.clone());
                }
                SessionCommand::SubAgentStatus { id, reply, .. } => {
                    let _ = reply.send(Ok(match id {
                        Some(id) => format!("subagent \"w\" ({id}) — completed, depth 1"),
                        None => "- \"w\" [running]\n".to_string(),
                    }));
                }
                _ => {}
            }
            CommandEffect::none()
        }
    }

    fn toolbox(spawn_result: Result<Uuid, String>) -> SubAgentToolbox {
        let session = spawn_root(
            StubSession { spawn_result },
            Arc::new(InMemoryJournal::new()),
        );
        SubAgentToolbox::new(Arc::new(EmptyToolbox), session, SubAgentParent::Main)
    }

    #[tokio::test]
    async fn specs_advertise_both_tools() {
        let names: Vec<String> = toolbox(Ok(Uuid::new_v4()))
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&SPAWN_AGENT_TOOL.to_string()));
        assert!(names.contains(&SUBAGENT_STATUS_TOOL.to_string()));
    }

    #[tokio::test]
    async fn spawn_returns_the_new_id() {
        let id = Uuid::new_v4();
        let out = toolbox(Ok(id))
            .execute(
                SPAWN_AGENT_TOOL,
                json!({"label": "research", "task": "dig"}),
            )
            .await
            .unwrap();
        assert_eq!(out, Value::String(format!("Subagent spawned: {id}")));
    }

    #[tokio::test]
    async fn spawn_surfaces_limit_errors_as_tool_errors() {
        let err = toolbox(Err("8 subagents already active".into()))
            .execute(SPAWN_AGENT_TOOL, json!({"label": "x", "task": "y"}))
            .await
            .unwrap_err();
        match err {
            ToolCallError::ExecutionFailed(msg) => assert!(msg.contains("8 subagents")),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_requires_label_and_task() {
        let err = toolbox(Ok(Uuid::new_v4()))
            .execute(SPAWN_AGENT_TOOL, json!({"label": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn status_with_and_without_id() {
        let tb = toolbox(Ok(Uuid::new_v4()));
        let id = Uuid::new_v4();
        let one = tb
            .execute(SUBAGENT_STATUS_TOOL, json!({"id": id.to_string()}))
            .await
            .unwrap();
        assert!(one.as_str().unwrap().contains("completed"));
        let all = tb.execute(SUBAGENT_STATUS_TOOL, json!({})).await.unwrap();
        assert!(all.as_str().unwrap().contains("[running]"));
        let err = tb
            .execute(SUBAGENT_STATUS_TOOL, json!({"id": "not-a-uuid"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn delegates_other_tools_to_inner() {
        let err = toolbox(Ok(Uuid::new_v4()))
            .execute("bash", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }
}
