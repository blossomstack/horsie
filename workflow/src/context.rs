use crate::workflow_actor::{WorkflowCommand, WorkflowNotification};
use actor::ActorRef;
use agentcore::{EventSink, LlmProvider, ToolCallError, ToolSpec, Toolbox, ToolboxImpl};
use async_trait::async_trait;
use models::workflow::WorkflowAgentDef;
use runtime_client::{RuntimeClient, add_runtime_tools};
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

/// Name of the builtin tool that re-scans the workspace and returns the current
/// skill catalog (name + description). Always advertised, like `skill`.
pub const LIST_SKILLS_TOOL: &str = "list_skills";

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

/// Resources injected into an [`AgentActor`](crate::AgentActor) when a
/// [`WorkflowActor`](crate::WorkflowActor) spawns it.
#[derive(Clone)]
pub struct AgentRuntimeContext {
    pub provider: Arc<dyn LlmProvider>,
    /// Toolbox pre-filtered to the tools this agent is permitted to use, with the
    /// `conclude` tool layered on when the agent has an output schema and/or may ask.
    pub toolbox: Arc<dyn Toolbox>,
    pub event_sink: Arc<dyn EventSink>,
    pub parent_ref: ActorRef<WorkflowCommand>,
    pub session_id: Uuid,
}

/// Builds the toolbox an agent runs with: its permitted runtime tools plus the
/// synthesized `conclude` terminal tool.
pub trait ToolboxFactory: Send + Sync + 'static {
    fn for_agent(
        &self,
        agent_def: &WorkflowAgentDef,
        runtime_client: RuntimeClient,
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
                "Load the full instructions for a named skill (see 'Available skills' or list_skills)."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string", "description": "The skill name." } }
            }),
        });
        specs.push(ToolSpec {
            name: LIST_SKILLS_TOOL.to_string(),
            description:
                "Re-scan the workspace and list the skills currently available (name + description)."
                    .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
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
            let ws = crate::workspace::scan(&self.runtime_client).await;
            return match ws.skills.get(requested) {
                Some(skill) => Ok(Value::String(skill.body.clone())),
                None => Err(ToolCallError::InvalidInput(format!(
                    "unknown skill '{requested}'; available: {}",
                    ws.skills.names().join(", ")
                ))),
            };
        }
        if name == LIST_SKILLS_TOOL {
            let ws = crate::workspace::scan(&self.runtime_client).await;
            return Ok(Value::String(crate::workspace::list_skills_result(
                &ws.skills,
            )));
        }
        self.base.execute(name, input).await
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
    use runtime_client::MockTransport;

    fn def(allowed: Option<Vec<String>>, output: Option<Value>, ask: bool) -> WorkflowAgentDef {
        WorkflowAgentDef {
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

    fn scan_with_skill() -> models::runtime::WorkspaceScan {
        let content = "---\nname: git-bisect\ndescription: find bad commit\n---\nStep 1...";
        models::runtime::WorkspaceScan {
            instructions: None,
            skills: vec![models::runtime::ScannedFile {
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
        let tb = DefaultToolboxFactory
            .for_agent(&def(Some(vec!["bash".into()]), Some(out), false), client);
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&CONCLUDE_TOOL.to_string()));
        assert!(!names.contains(&"read_file".to_string()));
    }

    #[tokio::test]
    async fn conclude_tool_is_not_executable() {
        let client = RuntimeClient::new(MockTransport::ok(""));
        let out = json!({"type": "object"});
        let tb = DefaultToolboxFactory.for_agent(&def(None, Some(out), false), client);
        let err = tb.execute(CONCLUDE_TOOL, json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn skill_and_list_skills_always_present() {
        let client = RuntimeClient::new(MockTransport::ok("")); // empty scan
        let tb = DefaultToolboxFactory.for_agent(&def(None, None, false), client);
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&SKILL_TOOL.to_string()));
        assert!(names.contains(&LIST_SKILLS_TOOL.to_string()));
    }

    #[tokio::test]
    async fn skill_fetches_live_and_list_skills_reports() {
        let client = RuntimeClient::new(MockTransport::ok("").with_scan(scan_with_skill()));
        let tb = DefaultToolboxFactory.for_agent(&def(None, None, false), client);

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

        let listed = tb.execute(LIST_SKILLS_TOOL, json!({})).await.unwrap();
        assert_eq!(
            listed,
            json!("1 skills available:\n- git-bisect: find bad commit")
        );
    }
}
