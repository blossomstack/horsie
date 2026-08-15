//! Server-owned tools for delegating work: `spawn_agent` starts a subagent
//! and `subagent_status` inspects the caller's subtree. Both route through the
//! owning session's mailbox — the session is the one place that enforces
//! limits, persists the tree, and owns the child actors.
//!
//! Layered onto every agent in a session, main and sub alike (which is what
//! makes sub-spawning work), carrying the *calling* agent's identity so
//! spawns are attributed to the right parent.

use crate::agent_loop::AgentCatalog;
use crate::sessions::addressing::SessionRef;
use crate::sessions::runners::ids::AgentId;
use crate::sessions::session_actor::SessionCommand;
use crate::sessions::session_actor::SubAgentCommand;
use crate::sessions::subagents::SubAgentParent;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Name of the built-in subagent-spawning tool.
pub const SPAWN_AGENT_TOOL: &str = "spawn_agent";
/// Name of the built-in subagent-inspection tool.
pub const SUBAGENT_STATUS_TOOL: &str = "subagent_status";

fn spawn_agent_spec(catalog: &AgentCatalog) -> ToolSpec {
    let mut description = "Spawn a subagent to work on a task independently and in parallel. \
        Returns immediately with the subagent's id; its result or failure is automatically \
        delivered back to you as a message. Continue with independent work, or wait if none \
        remains; do not poll subagent_status or call it repeatedly. Spawning fails when the \
        session's subagent limits (depth or concurrency) are reached."
        .to_string();
    let mut properties = serde_json::Map::new();
    properties.insert(
        "label".to_string(),
        json!({
            "type": "string",
            "description": "A short human-readable label for the subagent (a few words)."
        }),
    );
    properties.insert(
        "task".to_string(),
        json!({
            "type": "string",
            "description": "The complete, self-contained task for the subagent. It \
                inherits your model and tools but not your conversation — include \
                everything it needs to know."
        }),
    );
    // The catalogue goes in the description, not in a JSON `enum`: a bare list
    // of names says nothing about when to pick one, and `description` is the
    // whole point of the frontmatter field. With no agents installed the
    // parameter is absent entirely, so a session with no plugins sees exactly
    // the tool it saw before they existed.
    if !catalog.is_empty() {
        let listing = catalog
            .iter()
            .map(|a| format!("- {}: {}", a.def.name, a.def.description))
            .collect::<Vec<_>>()
            .join("\n");
        description.push_str(&format!(
            "\n\nInstalled agent types, each with its own instructions, tools and \
             expertise. Pass one as `agent_type` when its description fits the task \
             better than a general-purpose subagent would:\n{listing}"
        ));
        properties.insert(
            "agent_type".to_string(),
            json!({
                "type": "string",
                "description": "Name of an installed agent type, from the list above. \
                    Omit for a general-purpose subagent that inherits your own \
                    instructions and tools."
            }),
        );
    }
    ToolSpec {
        name: SPAWN_AGENT_TOOL.to_string(),
        description,
        input_schema: json!({
            "type": "object",
            "required": ["label", "task"],
            "properties": properties,
        }),
    }
}

fn subagent_status_spec() -> ToolSpec {
    ToolSpec {
        name: SUBAGENT_STATUS_TOOL.to_string(),
        description: "Inspect subagent status only for a user-requested progress update or \
            to diagnose a suspected runtime or result-delivery problem. Do not poll or call \
            this tool repeatedly: terminal results and failures are automatically delivered \
            to you as messages. With `id`, returns that subagent's status and its output or \
            error once finished. Without `id`, lists your whole subagent subtree with \
            statuses."
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
    session: SessionRef,
    /// Which agent this toolbox belongs to — the parent spawns attribute to.
    caller: SubAgentParent,
    /// The same agent in the runners' flat id space. Carried beside `caller`
    /// because [`SubAgentParent`] cannot tell a main agent, a step and a fork
    /// apart, so a runner handed only `caller` could not find who called.
    agent: AgentId,
    /// The plugin-declared agents this session can spawn. Held here because
    /// this toolbox is built in `provide()`, where the library scan is; the
    /// session actor never learns what an agent type is, it journals a string.
    catalog: Arc<AgentCatalog>,
}

impl SubAgentToolbox {
    pub fn new(
        inner: Arc<dyn Toolbox>,
        session: SessionRef,
        caller: SubAgentParent,
        agent: AgentId,
        catalog: Arc<AgentCatalog>,
    ) -> Self {
        Self {
            inner,
            session,
            caller,
            agent,
            catalog,
        }
    }

    /// Validate a requested agent type against the catalogue.
    ///
    /// Rejected here rather than at the session, because this is the layer that
    /// advertised the list — an error naming what exists is only possible where
    /// the list is.
    fn resolve_type(&self, input: &Value) -> Result<Option<String>, ToolCallError> {
        let Some(requested) = input.get("agent_type").and_then(Value::as_str) else {
            return Ok(None);
        };
        let requested = requested.trim();
        if requested.is_empty() {
            return Ok(None);
        }
        if self.catalog.get(requested).is_some() {
            return Ok(Some(requested.to_string()));
        }
        let known = self.catalog.names();
        Err(ToolCallError::InvalidInput(if known.is_empty() {
            format!("no agent type '{requested}': this session has no agent types installed")
        } else {
            format!(
                "no agent type '{requested}'; installed types are {}",
                known.join(", ")
            )
        }))
    }
}

#[async_trait]
impl Toolbox for SubAgentToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(spawn_agent_spec(&self.catalog));
        specs.push(subagent_status_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name == SPAWN_AGENT_TOOL {
            let label = input
                .get("label")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolCallError::InvalidInput("missing 'label'".to_string()))?;
            let task = input
                .get("task")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolCallError::InvalidInput("missing 'task'".to_string()))?;
            let agent_type = self.resolve_type(&input)?;
            let id = self
                .session
                .ask(|reply| {
                    SessionCommand::SubAgent(SubAgentCommand::Spawn {
                        caller: self.caller,
                        agent: self.agent,
                        label: label.to_string(),
                        task: task.to_string(),
                        agent_type,
                        reply,
                    })
                })
                .await
                .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
                .map_err(ToolCallError::ExecutionFailed)?;
            return Ok(ToolOutcome::Result(Value::String(format!(
                "Subagent spawned: {id}"
            ))));
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
                .ask(|reply| {
                    SessionCommand::SubAgent(SubAgentCommand::Status {
                        caller: self.caller,
                        agent: self.agent,
                        id,
                        reply,
                    })
                })
                .await
                .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
                .map_err(ToolCallError::ExecutionFailed)?;
            return Ok(ToolOutcome::Result(Value::String(rendered)));
        }
        self.inner.execute(name, input, tool_call_id).await
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

    #[derive(Serialize, Deserialize, Default)]
    struct Empty;

    /// Answers spawn/status asks the way the session will, so tool behavior
    /// is tested without a session actor.
    struct StubSession {
        spawn_result: Result<Uuid, String>,
        /// Which agent each command named, in arrival order.
        seen: Arc<std::sync::Mutex<Vec<AgentId>>>,
    }

    #[async_trait::async_trait]
    impl EventSourcedActor for StubSession {
        type Command = SessionInbox;
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
            cmd: SessionInbox,
            _ctx: &mut ActorContext<SessionInbox>,
        ) -> CommandEffect<()> {
            match cmd.cmd {
                SessionCommand::SubAgent(SubAgentCommand::Spawn { agent, reply, .. }) => {
                    self.seen.lock().unwrap().push(agent);
                    let _ = reply.send(self.spawn_result.clone());
                }
                SessionCommand::SubAgent(SubAgentCommand::Status {
                    agent, id, reply, ..
                }) => {
                    self.seen.lock().unwrap().push(agent);
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
        with_catalog(spawn_result, AgentCatalog::default())
    }

    fn with_catalog(spawn_result: Result<Uuid, String>, catalog: AgentCatalog) -> SubAgentToolbox {
        built(spawn_result, catalog).0
    }

    /// The toolbox, the agent it was built for, and what the session was told.
    fn built(
        spawn_result: Result<Uuid, String>,
        catalog: AgentCatalog,
    ) -> (
        SubAgentToolbox,
        AgentId,
        Arc<std::sync::Mutex<Vec<AgentId>>>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let session = SessionRef::new(
            crate::testing::spawn_detached(
                &horsie_actor::ActorSystem::new(Arc::new(InMemoryJournal::new())),
                StubSession {
                    spawn_result,
                    seen: Arc::clone(&seen),
                },
            ),
            crate::auth::UserId::bootstrap(),
            Uuid::new_v4(),
            None,
        );
        let agent = AgentId::new_v4();
        (
            SubAgentToolbox::new(
                Arc::new(EmptyToolbox),
                session,
                SubAgentParent::Main,
                agent,
                Arc::new(catalog),
            ),
            agent,
            seen,
        )
    }

    /// A catalogue of one, built the way a real scan builds it.
    fn catalog_of(name: &str, description: &str) -> AgentCatalog {
        std::iter::once(crate::agent_loop::CatalogAgent {
            plugin: "fd".into(),
            def: horsie_support::plugin::agents::PluginAgentDef {
                name: name.into(),
                description: description.into(),
                model: None,
                tools: Vec::new(),
                prompt: "be one".into(),
            },
        })
        .collect()
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

    #[test]
    fn tool_descriptions_prohibit_status_polling() {
        let spawn = spawn_agent_spec(&AgentCatalog::default());
        assert!(
            spawn.description.contains("automatically delivered"),
            "{}",
            spawn.description
        );
        assert!(
            spawn.description.contains("do not poll"),
            "{}",
            spawn.description
        );

        let status = subagent_status_spec();
        assert!(
            status
                .description
                .contains("user-requested progress update"),
            "{}",
            status.description
        );
        assert!(
            status.description.contains("diagnos"),
            "{}",
            status.description
        );
        assert!(
            status.description.contains("Do not poll"),
            "{}",
            status.description
        );
    }

    /// A session with no plugin agents sees exactly the tool it saw before
    /// agent types existed — no vestigial parameter, no empty list to reason
    /// about.
    #[tokio::test]
    async fn with_no_agents_installed_the_parameter_is_absent() {
        let specs = toolbox(Ok(Uuid::new_v4())).specs();
        let spawn = specs.iter().find(|s| s.name == SPAWN_AGENT_TOOL).unwrap();
        assert!(spawn.input_schema["properties"]["agent_type"].is_null());
        assert!(!spawn.description.contains("agent_type"));
    }

    /// The catalogue rides in the description, because a name alone does not
    /// tell a model when to pick one.
    #[tokio::test]
    async fn an_installed_agent_is_offered_with_its_description() {
        let tb = with_catalog(
            Ok(Uuid::new_v4()),
            catalog_of("code-reviewer", "reviews diffs for real bugs"),
        );
        let specs = tb.specs();
        let spawn = specs.iter().find(|s| s.name == SPAWN_AGENT_TOOL).unwrap();
        assert!(spawn.input_schema["properties"]["agent_type"].is_object());
        assert!(
            spawn
                .description
                .contains("- code-reviewer: reviews diffs for real bugs"),
            "{}",
            spawn.description
        );
    }

    /// Rejected at the layer that advertised the list, so the error can name
    /// what actually exists.
    #[tokio::test]
    async fn an_unknown_agent_type_is_refused_and_names_the_known_ones() {
        let tb = with_catalog(Ok(Uuid::new_v4()), catalog_of("code-reviewer", "reviews"));
        let err = tb
            .execute(
                SPAWN_AGENT_TOOL,
                json!({"label": "x", "task": "y", "agent_type": "reviewer"}),
                "tc1",
            )
            .await
            .unwrap_err();
        match err {
            ToolCallError::InvalidInput(msg) => {
                assert!(msg.contains("code-reviewer"), "{msg}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// An omitted or blank type is the general-purpose subagent, not an error.
    #[tokio::test]
    async fn an_absent_agent_type_spawns_a_general_purpose_subagent() {
        let tb = with_catalog(Ok(Uuid::new_v4()), catalog_of("code-reviewer", "reviews"));
        for input in [
            json!({"label": "x", "task": "y"}),
            json!({"label": "x", "task": "y", "agent_type": "  "}),
        ] {
            assert!(tb.execute(SPAWN_AGENT_TOOL, input, "tc1").await.is_ok());
        }
    }

    #[tokio::test]
    async fn spawn_returns_the_new_id() {
        let id = Uuid::new_v4();
        let out = toolbox(Ok(id))
            .execute(
                SPAWN_AGENT_TOOL,
                json!({"label": "research", "task": "dig"}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(
            out.expect_value(),
            Value::String(format!("Subagent spawned: {id}"))
        );
    }

    #[tokio::test]
    async fn spawn_surfaces_limit_errors_as_tool_errors() {
        let err = toolbox(Err("8 subagents already active".into()))
            .execute(SPAWN_AGENT_TOOL, json!({"label": "x", "task": "y"}), "tc1")
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
            .execute(SPAWN_AGENT_TOOL, json!({"label": "x"}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn status_with_and_without_id() {
        let tb = toolbox(Ok(Uuid::new_v4()));
        let id = Uuid::new_v4();
        let one = tb
            .execute(SUBAGENT_STATUS_TOOL, json!({"id": id.to_string()}), "tc1")
            .await
            .unwrap();
        assert!(one.expect_value().as_str().unwrap().contains("completed"));
        let all = tb
            .execute(SUBAGENT_STATUS_TOOL, json!({}), "tc1")
            .await
            .unwrap();
        assert!(all.expect_value().as_str().unwrap().contains("[running]"));
        let err = tb
            .execute(SUBAGENT_STATUS_TOOL, json!({"id": "not-a-uuid"}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    /// Both commands name the agent that called the tool. `caller` cannot say
    /// it: a main agent, a workflow step and a fork all collapse into
    /// `SubAgentParent::Main`, so without this field the session has no way to
    /// tell which of them spawned.
    #[tokio::test]
    async fn both_commands_carry_the_calling_agent() {
        let (tb, agent, seen) = built(Ok(Uuid::new_v4()), AgentCatalog::default());
        tb.execute(SPAWN_AGENT_TOOL, json!({"label": "x", "task": "y"}), "tc1")
            .await
            .unwrap();
        tb.execute(SUBAGENT_STATUS_TOOL, json!({}), "tc2")
            .await
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![agent, agent]);
    }

    #[tokio::test]
    async fn delegates_other_tools_to_inner() {
        let err = toolbox(Ok(Uuid::new_v4()))
            .execute("bash", json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }
}
