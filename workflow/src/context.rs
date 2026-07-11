use crate::workflow_actor::WorkflowNotification;
use async_trait::async_trait;
use horsie_agentcore::{EventSink, LlmProvider, ToolCallError, ToolSpec, Toolbox, ToolboxImpl};
use horsie_models::workflow::WorkflowAgentDef;
use horsie_runtime_client::{RuntimeClient, add_runtime_tools};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

/// Name of the builtin terminal tool an agent calls to finish its turn — either
/// delivering its structured output or asking the user a question.
pub const CONCLUDE_TOOL: &str = "conclude";

/// Name of the builtin tool an agent calls to load a skill's full instructions on
/// demand (progressive disclosure). Always advertised; re-scans the workspace live.
pub const SKILL_TOOL: &str = "skill";

/// Name of the builtin tool that re-scans the workspace(s) and returns the current
/// catalog (path, git status, instruction presence, skills). Always advertised, like
/// `skill`. Replaces the former `list_skills`.
pub const INSPECT_WORKSPACE_TOOL: &str = "inspect_workspace";

/// Resources injected into a [`WorkflowActor`](crate::WorkflowActor) at construction.
///
/// These are runtime wiring, not persisted state — they are recreated on every
/// spawn or restart and never written to the journal.
#[derive(Clone)]
pub struct WorkflowRuntimeContext {
    /// LLM providers keyed by the `model` field of a [`WorkflowAgentDef`].
    pub provider_registry: HashMap<String, Arc<dyn LlmProvider>>,
    /// Builds a per-agent toolbox, applying the agent's tool allowlist and the
    /// synthesized `conclude` tool.
    pub toolbox_factory: Arc<dyn ToolboxFactory>,
    /// Client for executing tools inside a managed runtime.
    pub runtime_client: RuntimeClient,
    /// Sink for streaming observation events (never journaled).
    pub event_sink: Arc<dyn EventSink>,
    /// Live push channel for workflow status transitions (never journaled).
    pub workflow_events: tokio::sync::mpsc::Sender<WorkflowNotification>,
}

impl WorkflowRuntimeContext {
    /// Resolve the provider for an agent's `model` key.
    pub fn provider_for(&self, model: &str) -> Option<Arc<dyn LlmProvider>> {
        self.provider_registry.get(model).cloned()
    }
}

/// A terminal outcome an [`AgentActor`](crate::AgentActor) reports to whoever
/// spawned it — the workflow that orchestrates it, or an interactive session.
#[derive(Debug, Clone)]
pub enum AgentOutcome {
    /// The agent produced its output (structured, or its final text).
    Concluded { session_id: Uuid, output: Value },
    /// The agent paused to ask the user a question.
    Asked {
        session_id: Uuid,
        tool_call_id: Option<String>,
        question: String,
    },
    /// The agent parked itself awaiting its timers.
    Parked { session_id: Uuid },
    /// The agent run failed.
    Failed {
        session_id: Uuid,
        error: String,
        recoverable: bool,
    },
}

/// Where an [`AgentActor`](crate::AgentActor) delivers its [`AgentOutcome`].
/// Implemented by the workflow (mapping outcomes into its own commands) and by
/// the session server; keeps the agent decoupled from any one parent's command
/// enum.
#[async_trait]
pub trait AgentOutcomeSink: Send + Sync {
    async fn deliver(&self, outcome: AgentOutcome);
}

/// Resources injected into an [`AgentActor`](crate::AgentActor) when a
/// [`WorkflowActor`](crate::WorkflowActor) spawns it.
#[derive(Clone)]
pub struct AgentRuntimeContext {
    pub provider: Arc<dyn LlmProvider>,
    /// Toolbox pre-filtered to the tools this agent is permitted to use, with the
    /// `conclude` tool layered on when the agent has an output schema and/or may ask.
    pub toolbox: Arc<dyn Toolbox>,
    pub event_sink: Arc<dyn EventSink>,
    /// Whoever spawned this agent; receives its terminal outcome.
    pub parent: Arc<dyn AgentOutcomeSink>,
    pub session_id: Uuid,
}

/// Builds the toolbox an agent runs with: its permitted runtime tools plus the
/// synthesized `conclude` terminal tool.
pub trait ToolboxFactory: Send + Sync + 'static {
    fn for_agent(
        &self,
        agent_def: &WorkflowAgentDef,
        runtime_client: RuntimeClient,
        workspace_names: Vec<String>,
        use_plugins: bool,
    ) -> Arc<dyn Toolbox>;
}

/// Default factory: exposes the standard runtime-backed tools narrowed to the
/// agent's allowlist, plus the `conclude` tool when applicable.
pub struct DefaultToolboxFactory;

impl ToolboxFactory for DefaultToolboxFactory {
    fn for_agent(
        &self,
        agent_def: &WorkflowAgentDef,
        runtime_client: RuntimeClient,
        workspace_names: Vec<String>,
        use_plugins: bool,
    ) -> Arc<dyn Toolbox> {
        let client = runtime_client.clone();
        let runtime = add_runtime_tools(ToolboxImpl::new(), runtime_client);
        let base: Arc<dyn Toolbox> = match &agent_def.allowed_tools {
            None => Arc::new(runtime),
            Some(list) => Arc::new(FilteredToolbox::new(
                Arc::new(runtime),
                list.iter().cloned().collect(),
            )),
        };
        // The timer tools themselves are layered on at run time by the AgentActor;
        // here we only widen the `conclude` schema to offer `park`.
        let conclude = conclude_tool_spec(
            agent_def.output_schema.as_ref(),
            agent_def.allow_ask_user,
            agent_def.allow_timers.unwrap_or(false),
        );
        Arc::new(AgentToolbox {
            base,
            conclude,
            runtime_client: client,
            workspace_names,
            use_plugins,
        })
    }
}

/// Synthesize the `conclude` tool's input schema for an agent. Returns `None` when
/// the agent neither produces structured output, may ask, nor uses timers (it then
/// ends its turn with a plain message).
///
/// With `allow_timers` the tool is always a `kind`-tagged union including `park`
/// (suspend awaiting timers) and `submit` (deliver output), plus `ask` when
/// permitted. Without timers, behavior is exactly as before.
pub fn conclude_tool_spec(
    output_schema: Option<&Value>,
    allow_ask: bool,
    allow_timers: bool,
) -> Option<ToolSpec> {
    let input_schema = if allow_timers {
        timers_kind_schema(output_schema, allow_ask)
    } else {
        match (output_schema, allow_ask) {
            (None, false) => return None,
            // Output only: the tool input *is* the output schema.
            (Some(out), false) => out.clone(),
            // Ask only: the tool input is a question (+ optional choices).
            (None, true) => ask_schema(),
            // Both: a `kind`-tagged union of submit-output and ask.
            (Some(out), true) => both_schema(out),
        }
    };
    Some(ToolSpec {
        name: CONCLUDE_TOOL.to_string(),
        description:
            "Finish your turn: deliver final output, ask the user, or park to await your timers."
                .to_string(),
        input_schema,
    })
}

/// Kind-tagged conclude schema for timer-capable agents. Always offers `submit`
/// and `park`; adds `ask` when permitted.
fn timers_kind_schema(output_schema: Option<&Value>, allow_ask: bool) -> Value {
    let mut kinds = vec![json!("submit"), json!("park")];
    if allow_ask {
        kinds.push(json!("ask"));
    }
    json!({
        "type": "object",
        "required": ["kind"],
        "properties": {
            "kind": {
                "type": "string",
                "enum": kinds,
                "description": "submit: deliver final output. park: suspend until a timer fires. ask: pause for user input."
            },
            "output": output_schema.cloned().unwrap_or_else(|| json!({})),
            "question": { "type": "string", "description": "Required when kind=ask." },
            "choices": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional when kind=ask."
            }
        }
    })
}

fn ask_schema() -> Value {
    json!({
        "type": "object",
        "required": ["question"],
        "properties": {
            "question": { "type": "string", "description": "The question to put to the user." },
            "choices": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional suggested answers."
            }
        }
    })
}

fn both_schema(output_schema: &Value) -> Value {
    json!({
        "type": "object",
        "required": ["kind"],
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["submit", "ask"],
                "description": "submit to deliver final output; ask to pause for user input"
            },
            "output": output_schema,
            "question": { "type": "string", "description": "Required when kind=ask." },
            "choices": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional when kind=ask."
            }
        }
    })
}

/// A toolbox = a base (permitted runtime tools), the optional `conclude` terminal
/// tool, and the always-present `skill` / `list_skills` tools. The latter two re-scan
/// the workspace live on each call (no cached skill set), so a skill added mid-run is
/// immediately loadable. `conclude`, `skill`, and `list_skills` bypass the allowlist.
struct AgentToolbox {
    base: Arc<dyn Toolbox>,
    conclude: Option<ToolSpec>,
    runtime_client: RuntimeClient,
    /// Names of the job's workspaces (stable for the job); used to apply the
    /// "optional iff single" rule and to list valid names in errors. The runtime owns
    /// the actual name→path resolution.
    workspace_names: Vec<String>,
    /// Whether this agent may see the shared plugin library (`horsie_shared`).
    use_plugins: bool,
}

impl AgentToolbox {
    /// Resolve the optional `workspace` argument of a skill-side tool to a concrete
    /// name. `None` is allowed only when there is exactly one workspace.
    fn resolve_workspace(&self, requested: Option<&str>) -> Result<String, ToolCallError> {
        match requested {
            Some(name) => {
                if self.workspace_names.iter().any(|n| n == name) {
                    Ok(name.to_string())
                } else {
                    Err(ToolCallError::InvalidInput(format!(
                        "unknown workspace '{name}'; available: {}",
                        self.workspace_names.join(", ")
                    )))
                }
            }
            None => match self.workspace_names.as_slice() {
                [only] => Ok(only.clone()),
                _ => Err(ToolCallError::InvalidInput(format!(
                    "specify a workspace: {}",
                    self.workspace_names.join(", ")
                ))),
            },
        }
    }
}

#[async_trait]
impl Toolbox for AgentToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.base.specs();
        if let Some(c) = &self.conclude {
            specs.push(c.clone());
        }
        specs.push(ToolSpec {
            name: SKILL_TOOL.to_string(),
            description:
                "Load the full instructions for a named skill in a workspace (see '# Workspaces' or inspect_workspace)."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "description": "The skill name." },
                    "workspace": { "type": "string", "description": "Which workspace the skill belongs to (see '# Workspaces'). Required when there is more than one workspace." }
                }
            }),
        });
        specs.push(ToolSpec {
            name: INSPECT_WORKSPACE_TOOL.to_string(),
            description:
                "Re-scan and show the current state of the workspace(s): path, git status, instruction-file presence, and available skills (name + description)."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Limit to one workspace (see '# Workspaces'). Omit to show all." }
                }
            }),
        });
        specs
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        if let Some(c) = &self.conclude
            && name == c.name
        {
            return Err(ToolCallError::ExecutionFailed(
                "the conclude tool is terminal and is not executed".to_string(),
            ));
        }
        if name == SKILL_TOOL {
            let requested = input
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let requested_ws = input.get("workspace").and_then(Value::as_str);
            // Shared plugin library: addressed by the reserved `horsie_shared` name,
            // resolved against the shared skill set (not a job workspace).
            if requested_ws == Some(crate::workspace::SHARED_WORKSPACE) {
                if !self.use_plugins {
                    return Err(ToolCallError::InvalidInput(
                        "the shared plugin library 'horsie_shared' is not enabled for this agent"
                            .to_string(),
                    ));
                }
                let (_, shared) = crate::workspace::scan(&self.runtime_client, None, true).await;
                return match shared.get(requested) {
                    Some(skill) => Ok(Value::String(shared_skill_body(skill))),
                    None => Err(ToolCallError::InvalidInput(format!(
                        "unknown shared skill '{requested}'; available: {}",
                        shared.names().join(", ")
                    ))),
                };
            }
            let ws_name = self.resolve_workspace(requested_ws)?;
            let (ws, _) =
                crate::workspace::scan(&self.runtime_client, Some(ws_name.clone()), false).await;
            let Some(info) = ws.find(&ws_name) else {
                return Err(ToolCallError::InvalidInput(format!(
                    "workspace '{ws_name}' is not available"
                )));
            };
            return match info.skills.get(requested) {
                Some(skill) => Ok(Value::String(skill.body.clone())),
                None => Err(ToolCallError::InvalidInput(format!(
                    "unknown skill '{requested}' in workspace '{ws_name}'; available: {}",
                    info.skills.names().join(", ")
                ))),
            };
        }
        if name == INSPECT_WORKSPACE_TOOL {
            let filter = input
                .get("workspace")
                .and_then(Value::as_str)
                .map(str::to_string);
            // Shared-only view.
            if filter.as_deref() == Some(crate::workspace::SHARED_WORKSPACE) {
                if !self.use_plugins {
                    return Err(ToolCallError::InvalidInput(
                        "the shared plugin library 'horsie_shared' is not enabled for this agent"
                            .to_string(),
                    ));
                }
                let (_, shared) = crate::workspace::scan(&self.runtime_client, None, true).await;
                return Ok(Value::String(crate::workspace::shared_inspect(&shared)));
            }
            let (ws, shared) =
                crate::workspace::scan(&self.runtime_client, filter.clone(), self.use_plugins)
                    .await;
            let mut out = crate::workspace::inspect_result(&ws);
            // Append the shared library when listing everything for an opted-in agent.
            if self.use_plugins && filter.is_none() {
                out.push_str("\n\n");
                out.push_str(&crate::workspace::shared_inspect(&shared));
            }
            return Ok(Value::String(out));
        }
        self.base.execute(name, input).await
    }
}

/// A shared skill's body plus a hint pointing at its directory under `horsie_shared`
/// so the agent can read sibling resources with the filesystem tools.
fn shared_skill_body(skill: &crate::workspace::Skill) -> String {
    match &skill.rel_dir {
        Some(dir) => format!(
            "{}\n\n[resources] This skill's files are under workspace \"{}\" at {}/. \
             Read one with read_file(workspace=\"{}\", path=\"{}/<file>\").",
            skill.body,
            crate::workspace::SHARED_WORKSPACE,
            dir,
            crate::workspace::SHARED_WORKSPACE,
            dir,
        ),
        None => skill.body.clone(),
    }
}

/// Wraps a toolbox and exposes only an allowlisted subset of its tools.
struct FilteredToolbox {
    inner: Arc<dyn Toolbox>,
    allowed: HashSet<String>,
}

impl FilteredToolbox {
    fn new(inner: Arc<dyn Toolbox>, allowed: HashSet<String>) -> Self {
        Self { inner, allowed }
    }
}

#[async_trait]
impl Toolbox for FilteredToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner
            .specs()
            .into_iter()
            .filter(|s| self.allowed.contains(&s.name))
            .collect()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        if !self.allowed.contains(name) {
            return Err(ToolCallError::InvalidInput(format!(
                "tool '{name}' is not permitted for this agent"
            )));
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
    use horsie_runtime_client::MockTransport;

    fn def(allowed: Option<Vec<String>>, output: Option<Value>, ask: bool) -> WorkflowAgentDef {
        WorkflowAgentDef {
            use_plugins: None,
            name: "a".into(),
            system_prompt: None,
            model: "m".into(),
            output_schema: output,
            allow_ask_user: ask,
            allow_timers: None,
            transitions: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: allowed,
        }
    }

    fn scan_with_skill(name: &str) -> horsie_models::runtime::WorkspaceScan {
        let content = "---\nname: git-bisect\ndescription: find bad commit\n---\nStep 1...";
        horsie_models::runtime::WorkspaceScan {
            name: name.into(),
            path: format!("/ws/{name}"),
            is_git_repo: false,
            instructions: None,
            skills: vec![horsie_models::runtime::ScannedFile {
                path: ".claude/skills/git-bisect/SKILL.md".into(),
                content: content.into(),
            }],
        }
    }

    #[test]
    fn conclude_not_registered_without_output_or_ask() {
        assert!(conclude_tool_spec(None, false, false).is_none());
    }

    #[test]
    fn conclude_output_only_uses_output_schema_as_input() {
        let out = json!({"type": "object", "properties": {"answer": {"type": "number"}}});
        let spec = conclude_tool_spec(Some(&out), false, false).unwrap();
        assert_eq!(spec.input_schema, out);
    }

    #[test]
    fn conclude_ask_only_requires_question() {
        let spec = conclude_tool_spec(None, true, false).unwrap();
        assert_eq!(spec.input_schema["required"][0], "question");
    }

    #[test]
    fn conclude_both_is_kind_tagged() {
        let out = json!({"type": "object"});
        let spec = conclude_tool_spec(Some(&out), true, false).unwrap();
        assert_eq!(spec.input_schema["properties"]["kind"]["enum"][0], "submit");
    }

    #[test]
    fn conclude_without_timers_is_unchanged() {
        // Backward-compat: the no-timers signature still returns None when neither
        // output nor ask is set.
        assert!(conclude_tool_spec(None, false, false).is_none());
    }

    #[test]
    fn conclude_with_timers_offers_park_and_submit() {
        let out = json!({"type": "object"});
        let spec = conclude_tool_spec(Some(&out), false, true).unwrap();
        let kinds: Vec<&str> = spec.input_schema["properties"]["kind"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(kinds.contains(&"submit"));
        assert!(kinds.contains(&"park"));
        assert!(!kinds.contains(&"ask"));
    }

    #[test]
    fn conclude_with_timers_and_ask_offers_all_three() {
        let out = json!({"type": "object"});
        let spec = conclude_tool_spec(Some(&out), true, true).unwrap();
        let kinds: Vec<&str> = spec.input_schema["properties"]["kind"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for k in ["submit", "ask", "park"] {
            assert!(kinds.contains(&k), "missing kind {k}");
        }
    }

    #[test]
    fn toolbox_includes_conclude_and_filters_runtime_tools() {
        let client = RuntimeClient::new(MockTransport::ok(""));
        let out = json!({"type": "object"});
        let tb = DefaultToolboxFactory.for_agent(
            &def(Some(vec!["bash".into()]), Some(out), false),
            client,
            vec!["october".into()],
            false,
        );
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&CONCLUDE_TOOL.to_string()));
        assert!(!names.contains(&"read_file".to_string()));
    }

    #[tokio::test]
    async fn conclude_tool_is_not_executable() {
        let client = RuntimeClient::new(MockTransport::ok(""));
        let out = json!({"type": "object"});
        let tb = DefaultToolboxFactory.for_agent(
            &def(None, Some(out), false),
            client,
            vec!["october".into()],
            false,
        );
        let err = tb.execute(CONCLUDE_TOOL, json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn skill_and_inspect_always_present() {
        let client = RuntimeClient::new(MockTransport::ok("")); // empty scan
        let tb = DefaultToolboxFactory.for_agent(
            &def(None, None, false),
            client,
            vec!["october".into()],
            false,
        );
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&SKILL_TOOL.to_string()));
        assert!(names.contains(&INSPECT_WORKSPACE_TOOL.to_string()));
    }

    #[tokio::test]
    async fn skill_fetches_live_for_single_workspace_default() {
        let client =
            RuntimeClient::new(MockTransport::ok("").with_scan(vec![scan_with_skill("october")]));
        let tb = DefaultToolboxFactory.for_agent(
            &def(None, None, false),
            client,
            vec!["october".into()],
            false,
        );

        // Single workspace → `workspace` may be omitted.
        let body = tb
            .execute(SKILL_TOOL, json!({ "name": "git-bisect" }))
            .await
            .unwrap();
        assert_eq!(body, json!("Step 1..."));

        let err = tb
            .execute(SKILL_TOOL, json!({ "name": "nope" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));

        let listed = tb.execute(INSPECT_WORKSPACE_TOOL, json!({})).await.unwrap();
        let text = listed.as_str().unwrap();
        assert!(text.contains("## october — /ws/october"));
        assert!(text.contains("- git-bisect: find bad commit"));
    }

    #[tokio::test]
    async fn skill_requires_workspace_when_multiple() {
        let client =
            RuntimeClient::new(MockTransport::ok("").with_scan(vec![scan_with_skill("october")]));
        let tb = DefaultToolboxFactory.for_agent(
            &def(None, None, false),
            client,
            vec!["alpha".into(), "beta".into()],
            false,
        );
        // Omitting `workspace` with several workspaces is rejected before any scan.
        let err = tb
            .execute(SKILL_TOOL, json!({ "name": "git-bisect" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
        // An unknown workspace name is also rejected.
        let err = tb
            .execute(
                SKILL_TOOL,
                json!({ "name": "git-bisect", "workspace": "zzz" }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    fn shared_skill() -> horsie_models::runtime::PluginSkill {
        horsie_models::runtime::PluginSkill {
            plugin: "sp".into(),
            rel_dir: "sp/skills/brainstorming".into(),
            content: "---\nname: brainstorming\ndescription: explore first\n---\nDo it.".into(),
        }
    }

    #[tokio::test]
    async fn shared_skill_loads_with_resource_hint_when_opted_in() {
        let client =
            RuntimeClient::new(MockTransport::ok("").with_shared_skills(vec![shared_skill()]));
        let tb = DefaultToolboxFactory.for_agent(
            &def(None, None, false),
            client,
            vec!["october".into()],
            true,
        );
        let body = tb
            .execute(
                SKILL_TOOL,
                json!({ "name": "brainstorming", "workspace": "horsie_shared" }),
            )
            .await
            .unwrap();
        let text = body.as_str().unwrap();
        assert!(text.contains("Do it."));
        assert!(text.contains("workspace=\"horsie_shared\""));
        assert!(text.contains("sp/skills/brainstorming"));
    }

    #[tokio::test]
    async fn shared_skill_rejected_when_opted_out() {
        let client =
            RuntimeClient::new(MockTransport::ok("").with_shared_skills(vec![shared_skill()]));
        let tb = DefaultToolboxFactory.for_agent(
            &def(None, None, false),
            client,
            vec!["october".into()],
            false,
        );
        let err = tb
            .execute(
                SKILL_TOOL,
                json!({ "name": "brainstorming", "workspace": "horsie_shared" }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn inspect_includes_shared_section_only_when_opted_in() {
        let client =
            RuntimeClient::new(MockTransport::ok("").with_shared_skills(vec![shared_skill()]));
        let tb = DefaultToolboxFactory.for_agent(
            &def(None, None, false),
            client.clone(),
            vec!["october".into()],
            true,
        );
        let out = tb.execute(INSPECT_WORKSPACE_TOOL, json!({})).await.unwrap();
        let text = out.as_str().unwrap();
        assert!(text.contains("## horsie_shared"));
        assert!(text.contains("- brainstorming: explore first"));

        // Opted-out agent never sees the shared section.
        let tb_off = DefaultToolboxFactory.for_agent(
            &def(None, None, false),
            client,
            vec!["october".into()],
            false,
        );
        let out = tb_off
            .execute(INSPECT_WORKSPACE_TOOL, json!({}))
            .await
            .unwrap();
        assert!(!out.as_str().unwrap().contains("horsie_shared"));
    }
}
