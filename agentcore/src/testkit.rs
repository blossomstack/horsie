//! Shared test doubles for the agent loop.
//!
//! Gated behind `cfg(any(test, feature = "test-util"))`: available to
//! agentcore's own unit tests unconditionally, and to other crates (the
//! provider conformance suite, workflow, server) when they enable
//! `horsie-agentcore/test-util`.

use crate::{
    error::{LlmError, ToolCallError},
    events::{EventSink, EventSinkError},
    provider::{CompletionRequest, CompletionResponse, LlmProvider, StopReason},
    tool::{ToolSpec, Toolbox},
};
use async_trait::async_trait;
use horsie_models::agent::{ContentPart, TextPart, ToolCallPart, Usage};
use horsie_models::events::AgentEvent;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, PoisonError};

/// An `LlmProvider` that replays a fixed list of responses, cycling when the
/// list is exhausted.
pub struct MockProvider {
    responses: Vec<CompletionResponse>,
    call_index: Mutex<usize>,
}

impl MockProvider {
    pub fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses,
            call_index: Mutex::new(0),
        })
    }

    pub fn text(text: &str) -> Arc<Self> {
        Self::new(vec![CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: text.to_string(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }])
    }

    pub fn tool_then_text(tool_id: &str, tool_name: &str, input: Value, reply: &str) -> Arc<Self> {
        Self::new(vec![
            CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: tool_id.to_string(),
                    name: tool_name.to_string(),
                    input,
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: reply.to_string(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 8,
                },
            },
        ])
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_id(&self) -> &str {
        "mock-model"
    }

    async fn complete(
        &self,
        _request: CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        let mut idx = self
            .call_index
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let response = self.responses[*idx % self.responses.len()].clone();
        *idx += 1;
        Ok(response)
    }
}

/// Executes a tool call by name. Returning `Err` exercises the loop's
/// tool-failure path.
pub type ToolHandler = Arc<dyn Fn(&str, Value) -> Result<Value, ToolCallError> + Send + Sync>;

/// A `Toolbox` backed by an arbitrary handler closure.
pub struct MockToolbox {
    specs: Vec<ToolSpec>,
    handler: ToolHandler,
}

impl MockToolbox {
    /// A toolbox advertising `specs`, dispatching every call to `handler`.
    pub fn new(specs: Vec<ToolSpec>, handler: ToolHandler) -> Arc<Self> {
        Arc::new(Self { specs, handler })
    }

    /// One tool named `name` that returns its input unchanged.
    pub fn echo(name: &str) -> Arc<Self> {
        let spec = ToolSpec {
            name: name.to_string(),
            description: "echo tool".to_string(),
            input_schema: json!({ "type": "object" }),
        };
        Arc::new(Self {
            specs: vec![spec],
            handler: Arc::new(|_, input| Ok(input)),
        })
    }
}

#[async_trait]
impl Toolbox for MockToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        (self.handler)(name, input)
    }
}

/// An `EventSink` that records every event for later assertion.
pub struct CollectingEventSink {
    events: Mutex<Vec<AgentEvent>>,
}

impl Default for CollectingEventSink {
    fn default() -> Self {
        Self::new()
    }
}

impl CollectingEventSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn message_complete_ids(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| {
                if let AgentEvent::MessageComplete(mc) = e {
                    Some(mc.message_id)
                } else {
                    None
                }
            })
            .collect()
    }
}

#[async_trait]
impl EventSink for CollectingEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
        Ok(())
    }
}
