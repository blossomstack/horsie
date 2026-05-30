use crate::workflow_actor::WorkflowCommand;
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
        let runtime = add_runtime_tools(ToolboxImpl::new(), runtime_client);
        let base: Arc<dyn Toolbox> = match &agent_def.allowed_tools {
            None => Arc::new(runtime),
            Some(list) => Arc::new(FilteredToolbox::new(
                Arc::new(runtime),
                list.iter().cloned().collect(),
            )),
        };
        let conclude =
            conclude_tool_spec(agent_def.output_schema.as_ref(), agent_def.allow_ask_user);
        Arc::new(AgentToolbox { base, conclude })
    }
}

/// Synthesize the `conclude` tool's input schema for an agent, or `None` when the
/// agent neither produces structured output nor may ask the user (in which case
/// the agent simply ends its turn with a plain message).
pub fn conclude_tool_spec(output_schema: Option<&Value>, allow_ask: bool) -> Option<ToolSpec> {
    let input_schema = match (output_schema, allow_ask) {
        (None, false) => return None,
        // Output only: the tool input *is* the output schema.
        (Some(out), false) => out.clone(),
        // Ask only: the tool input is a question (+ optional choices).
        (None, true) => ask_schema(),
        // Both: a `kind`-tagged union of submit-output and ask.
        (Some(out), true) => both_schema(out),
    };
    Some(ToolSpec {
        name: CONCLUDE_TOOL.to_string(),
        description:
            "Finish your turn: deliver your final structured output, or ask the user a question."
                .to_string(),
        input_schema,
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

/// A toolbox = a base (permitted runtime tools) plus the optional `conclude` tool,
/// which is advertised but never executed (the agent loop intercepts it).
struct AgentToolbox {
    base: Arc<dyn Toolbox>,
    conclude: Option<ToolSpec>,
}

#[async_trait]
impl Toolbox for AgentToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.base.specs();
        if let Some(c) = &self.conclude {
            specs.push(c.clone());
        }
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
            transitions: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: allowed,
        }
    }

    #[test]
    fn conclude_not_registered_without_output_or_ask() {
        assert!(conclude_tool_spec(None, false).is_none());
    }

    #[test]
    fn conclude_output_only_uses_output_schema_as_input() {
        let out = json!({"type": "object", "properties": {"answer": {"type": "number"}}});
        let spec = conclude_tool_spec(Some(&out), false).unwrap();
        assert_eq!(spec.input_schema, out);
    }

    #[test]
    fn conclude_ask_only_requires_question() {
        let spec = conclude_tool_spec(None, true).unwrap();
        assert_eq!(spec.input_schema["required"][0], "question");
    }

    #[test]
    fn conclude_both_is_kind_tagged() {
        let out = json!({"type": "object"});
        let spec = conclude_tool_spec(Some(&out), true).unwrap();
        assert_eq!(spec.input_schema["properties"]["kind"]["enum"][0], "submit");
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
}
