use crate::{
    error::{AgentBuildError, AgentError},
    events::EventSink,
    provider::{CompletionRequest, LlmProvider, StopReason, ToolChoice},
    tool::Toolbox,
};
use horsie_models::agent::{
    AgentInput, AgentOutput, AgentResult, CompletedOutput, ContentPart, HandoffOutput, Message,
    Role, ToolResultPart, Usage,
};
use horsie_models::events::{
    AgentEvent, InputMessageEvent, MessageCompleteEvent, MessageStartEvent, MessageStopEvent,
    RunCompleteEvent, ToolCompleteEvent, ToolExecutingEvent,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_iterations: u32,
    pub stuck_threshold: usize,
    pub nudge_threshold: usize,
    pub max_tokens: Option<u32>,
    /// How many times the model may be nudged to re-issue a malformed handoff call
    /// (called alongside other tools, or with input that fails schema validation)
    /// before the run fails.
    pub handoff_max_retries: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 100,
            stuck_threshold: 5,
            nudge_threshold: 3,
            max_tokens: None,
            handoff_max_retries: 2,
        }
    }
}

pub struct Agent {
    pub(crate) provider: Arc<dyn LlmProvider>,
    pub(crate) system_prompt: String,
    pub(crate) toolbox: Arc<dyn Toolbox>,
    /// The tool whose invocation ends the run and returns control to the caller as
    /// a `Handoff`, rather than being executed. Validated to exist in the toolbox
    /// at build time.
    pub(crate) handoff_tool: Option<String>,
    /// Whether the model is *forced* to call `handoff_tool` (`tool_choice: any`,
    /// and a text-only turn is nudged/rejected). `false` means the tool is offered
    /// but purely voluntary — the model may end its turn with plain text just as
    /// freely as with any other tool (`tool_choice: auto`); calling the tool is
    /// still recognized as a handoff rather than executed, for tools the agent may
    /// choose to use to pause (e.g. asking a clarifying question) rather than ones
    /// it must call to finish. See [`with_handoff_tool_optional`](AgentBuilder::with_handoff_tool_optional).
    pub(crate) force_handoff_choice: bool,
    /// Validator for the handoff tool's input, compiled from its declared
    /// `input_schema`. `None` when there is no handoff tool.
    pub(crate) handoff_validator: Option<Arc<jsonschema::Validator>>,
    pub(crate) config: AgentConfig,
    pub(crate) history: Vec<Message>,
}

pub struct AgentBuilder {
    provider: Arc<dyn LlmProvider>,
    system_prompt: String,
    toolbox: Arc<dyn Toolbox>,
    handoff_tool: Option<String>,
    force_handoff_choice: bool,
    config: AgentConfig,
    history: Vec<Message>,
}

impl AgentBuilder {
    pub fn new(provider: Arc<dyn LlmProvider>, toolbox: Arc<dyn Toolbox>) -> Self {
        Self {
            provider,
            system_prompt: String::new(),
            toolbox,
            handoff_tool: None,
            force_handoff_choice: true,
            config: AgentConfig::default(),
            history: Vec::new(),
        }
    }

    pub fn with_system_prompt(mut self, p: impl Into<String>) -> Self {
        self.system_prompt = p.into();
        self
    }

    /// Register the tool that ends the run as a `Handoff`. The model is forced to
    /// call it (`tool_choice: any`) — for a workflow sub-agent that must terminate
    /// via a specific tool. The tool must be present in the toolbox (checked by
    /// [`build`](Self::build)).
    pub fn with_handoff_tool(mut self, n: impl Into<String>) -> Self {
        self.handoff_tool = Some(n.into());
        self.force_handoff_choice = true;
        self
    }

    /// Like [`with_handoff_tool`](Self::with_handoff_tool), but the model is never
    /// forced to call it: `tool_choice` stays `auto`, and the model may end its
    /// turn with plain text exactly as if the tool weren't registered. A call to
    /// the tool is still recognized as a handoff (not executed as a regular tool
    /// call) when the model does choose to make one. For an interactive agent
    /// whose only "must finish" state is the next user message, and which may
    /// optionally pause to ask a clarifying question rather than being made to
    /// route every ordinary reply through a tool call.
    pub fn with_handoff_tool_optional(mut self, n: impl Into<String>) -> Self {
        self.handoff_tool = Some(n.into());
        self.force_handoff_choice = false;
        self
    }

    pub fn with_config(mut self, c: AgentConfig) -> Self {
        self.config = c;
        self
    }

    pub fn with_history(mut self, h: Vec<Message>) -> Self {
        self.history = h;
        self
    }

    pub fn build(self) -> Result<Agent, AgentBuildError> {
        if self.config.nudge_threshold >= self.config.stuck_threshold {
            return Err(AgentBuildError::InvalidConfig {
                nudge: self.config.nudge_threshold,
                stuck: self.config.stuck_threshold,
            });
        }

        // A handoff tool must be advertised in the toolbox — otherwise the model is
        // never told it exists and could never call it.
        let handoff_validator = match &self.handoff_tool {
            None => None,
            Some(name) => {
                let spec = self
                    .toolbox
                    .specs()
                    .into_iter()
                    .find(|s| &s.name == name)
                    .ok_or_else(|| AgentBuildError::HandoffToolNotRegistered {
                        tool: name.clone(),
                    })?;
                let validator = jsonschema::validator_for(&spec.input_schema).map_err(|e| {
                    AgentBuildError::InvalidHandoffSchema {
                        tool: name.clone(),
                        reason: e.to_string(),
                    }
                })?;
                Some(Arc::new(validator))
            }
        };

        Ok(Agent {
            provider: self.provider,
            system_prompt: self.system_prompt,
            toolbox: self.toolbox,
            handoff_tool: self.handoff_tool,
            force_handoff_choice: self.force_handoff_choice,
            handoff_validator,
            config: self.config,
            history: self.history,
        })
    }
}

fn extract_tool_calls(parts: &[ContentPart]) -> Vec<(String, String, Value)> {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolCall(tc) => Some((tc.id.clone(), tc.name.clone(), tc.input.clone())),
            ContentPart::Text(_) | ContentPart::ToolResult(_) | ContentPart::Thinking(_) => None,
        })
        .collect()
}

fn extract_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.text.as_str()),
            ContentPart::ToolCall(_) | ContentPart::ToolResult(_) | ContentPart::Thinking(_) => {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

fn tool_fingerprint(tool_calls: &[(String, String, Value)]) -> String {
    tool_calls
        .iter()
        .map(|(_, name, input)| format!("{name}:{input}"))
        .collect::<Vec<_>>()
        .join("|")
}

impl Agent {
    pub fn builder(provider: Arc<dyn LlmProvider>, toolbox: Arc<dyn Toolbox>) -> AgentBuilder {
        AgentBuilder::new(provider, toolbox)
    }

    /// Returns `Some(reason)` when a handoff call must be rejected: either it was
    /// issued alongside other tool calls, or its input fails the tool's schema.
    fn validate_handoff(
        &self,
        handoff_name: &str,
        tool_calls: &[(String, String, Value)],
        data: &Value,
    ) -> Option<String> {
        if tool_calls.len() > 1 {
            return Some(format!(
                "The '{handoff_name}' tool must be called on its own, with no other tool calls in the same turn."
            ));
        }
        if let Some(validator) = &self.handoff_validator
            && !validator.is_valid(data)
        {
            let detail = validator
                .validate(data)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "input does not match the tool's schema".to_string());
            return Some(format!("Invalid '{handoff_name}' input: {detail}"));
        }
        None
    }

    pub async fn run(
        &mut self,
        input: AgentInput,
        events: &dyn EventSink,
        cancel: CancellationToken,
    ) -> Result<AgentOutput, AgentError> {
        let run_id = Uuid::new_v4().to_string();

        let input_msg = input.to_message();
        events
            .emit(AgentEvent::InputMessage(InputMessageEvent {
                message_id: input.message_id(),
                input,
            }))
            .await?;
        self.history.push(input_msg);

        let mut total_usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        let mut iteration: u32 = 0;
        let mut recent_fingerprints: VecDeque<String> = VecDeque::new();
        let mut handoff_retries: u32 = 0;
        let mut handoff_missing_retries: u32 = 0;
        // With a *forced* handoff tool, require a tool call every turn (`Any`) so
        // the model can never end with bare text — it must keep working or call the
        // handoff tool to finish. Without one (or with an optional handoff tool —
        // see `with_handoff_tool_optional`), the model may end its turn with text
        // (`Auto`) exactly as freely as it may call any other tool. Honoring
        // `tool_choice` is a hard requirement of every provider.
        let tool_choice = if self.handoff_tool.is_some() && self.force_handoff_choice {
            ToolChoice::Any
        } else {
            ToolChoice::Auto
        };

        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            if iteration >= self.config.max_iterations {
                return Err(AgentError::MaxIterationsExceeded {
                    max: self.config.max_iterations,
                });
            }
            iteration += 1;

            let tools = self.toolbox.specs();
            let request = CompletionRequest {
                messages: &self.history,
                system: if self.system_prompt.is_empty() {
                    None
                } else {
                    Some(self.system_prompt.clone())
                },
                tools,
                tool_choice: tool_choice.clone(),
                max_tokens: self.config.max_tokens,
            };

            let msg_id = Uuid::new_v4().to_string();
            events
                .emit(AgentEvent::MessageStart(MessageStartEvent {
                    message_id: msg_id.clone(),
                    role: Role::Assistant,
                }))
                .await?;

            let response = self
                .provider
                .complete(request, &msg_id, events)
                .await
                .map_err(AgentError::Provider)?;

            events
                .emit(AgentEvent::MessageStop(MessageStopEvent {
                    message_id: msg_id.clone(),
                }))
                .await?;

            total_usage.input_tokens += response.usage.input_tokens;
            total_usage.output_tokens += response.usage.output_tokens;

            let assistant_msg = Message {
                id: msg_id.clone(),
                role: Role::Assistant,
                parts: response.parts.clone(),
            };
            events
                .emit(AgentEvent::MessageComplete(MessageCompleteEvent {
                    message_id: msg_id,
                    message: assistant_msg.clone(),
                }))
                .await?;
            self.history.push(assistant_msg);

            // A truncated turn is not a finished turn. Tool calls are exempt: a
            // backend may report `length` alongside a complete tool call, and the
            // loop can still execute it and continue.
            if response.stop_reason == StopReason::MaxTokens
                && extract_tool_calls(&response.parts).is_empty()
            {
                return Err(AgentError::Truncated {
                    max_tokens: self.config.max_tokens,
                });
            }

            let tool_calls = extract_tool_calls(&response.parts);

            if tool_calls.is_empty() {
                // A *forced* handoff agent must finish by *calling* its handoff tool,
                // never by ending its turn with plain text. `tool_choice: any` already
                // pushes the model toward a tool call; this is the safety net if a
                // provider returns text anyway — nudge and retry, then fail rather
                // than silently accept it. An optional handoff tool (`force_handoff_choice
                // = false`) skips this entirely: plain text is a perfectly normal way
                // to finish, exactly as if the tool weren't registered.
                if let Some(handoff_name) = self.handoff_tool.clone()
                    && self.force_handoff_choice
                {
                    if handoff_missing_retries >= self.config.handoff_max_retries {
                        return Err(AgentError::HandoffValidationFailed {
                            tool: handoff_name,
                            reason:
                                "model ended its turn with text instead of calling the handoff tool"
                                    .to_string(),
                        });
                    }
                    handoff_missing_retries += 1;
                    self.history.push(Message::user(
                        format!("nudge-handoff-missing:{iteration}"),
                        format!(
                            "You ended your turn without finishing. Call the '{handoff_name}' tool to \
                             deliver your output or ask the user; if there is more work to do, call a \
                             tool to do it."
                        ),
                    ));
                    continue;
                }

                events
                    .emit(AgentEvent::RunComplete(RunCompleteEvent {
                        message_id: run_id.clone(),
                        usage: total_usage.clone(),
                        iterations: iteration,
                    }))
                    .await?;
                return Ok(AgentOutput {
                    result: AgentResult::Completed(CompletedOutput {
                        text: extract_text(&response.parts),
                    }),
                    usage: total_usage,
                });
            }

            // Handoff handling: a call to the handoff tool ends the run, but only
            // once it is the sole tool call and its input passes schema validation.
            // Otherwise the model is nudged (via tool-result errors) to re-issue it,
            // bounded by `handoff_max_retries`.
            if let Some(handoff_name) = self.handoff_tool.clone()
                && let Some((_, _, data)) = tool_calls
                    .iter()
                    .find(|(_, n, _)| n == &handoff_name)
                    .cloned()
            {
                let rejection = self.validate_handoff(&handoff_name, &tool_calls, &data);
                match rejection {
                    None => {
                        events
                            .emit(AgentEvent::RunComplete(RunCompleteEvent {
                                message_id: run_id.clone(),
                                usage: total_usage.clone(),
                                iterations: iteration,
                            }))
                            .await?;
                        return Ok(AgentOutput {
                            result: AgentResult::Handoff(HandoffOutput {
                                tool_name: handoff_name,
                                data,
                            }),
                            usage: total_usage,
                        });
                    }
                    Some(reason) => {
                        if handoff_retries >= self.config.handoff_max_retries {
                            return Err(AgentError::HandoffValidationFailed {
                                tool: handoff_name,
                                reason,
                            });
                        }
                        handoff_retries += 1;
                        // Every tool_use in this turn needs a tool_result for the
                        // conversation to stay valid; tell the model what to fix.
                        for (tool_call_id, n, _) in &tool_calls {
                            let content = if n == &handoff_name {
                                reason.clone()
                            } else {
                                format!(
                                    "Ignored: call '{handoff_name}' on its own to finish, with no other tools."
                                )
                            };
                            self.history.push(Message::tool_result(
                                tool_call_id.clone(),
                                content,
                                true,
                            ));
                        }
                        continue;
                    }
                }
            }

            let fingerprint = tool_fingerprint(&tool_calls);
            recent_fingerprints.push_back(fingerprint.clone());
            if recent_fingerprints.len() > self.config.stuck_threshold {
                recent_fingerprints.pop_front();
            }

            if recent_fingerprints.len() >= self.config.stuck_threshold
                && recent_fingerprints.iter().all(|f| f == &fingerprint)
            {
                return Err(AgentError::StuckInLoop {
                    tool_name: tool_calls[0].1.clone(),
                    count: self.config.stuck_threshold,
                });
            }

            let should_nudge = recent_fingerprints.len() >= self.config.nudge_threshold
                && recent_fingerprints.iter().all(|f| f == &fingerprint);

            if should_nudge {
                for (tool_call_id, _, _) in &tool_calls {
                    let nudge_msg = Message {
                        id: format!("nudge:{tool_call_id}"),
                        role: Role::Tool,
                        parts: vec![ContentPart::ToolResult(ToolResultPart {
                            tool_call_id: tool_call_id.clone(),
                            output: "You have called this tool with identical arguments multiple times. Please try a different approach.".to_string(),
                            is_error: false,
                        })],
                    };
                    self.history.push(nudge_msg);
                }
                continue;
            }

            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            // Execute every tool call in this turn concurrently. The model may
            // request several at once (parallel tool use); running them in parallel
            // cuts a turn's latency to its slowest call rather than the sum. Results
            // are collected in request order so the history stays deterministic.
            let toolbox = &self.toolbox;
            let executions = tool_calls
                .iter()
                .map(|(tool_call_id, name, input)| async move {
                    let result_msg_id = format!("result:{tool_call_id}");

                    events
                        .emit(AgentEvent::ToolExecuting(ToolExecutingEvent {
                            message_id: result_msg_id.clone(),
                            tool_call_id: tool_call_id.clone(),
                        }))
                        .await?;

                    let (output, is_error) = match toolbox.execute(name, input.clone()).await {
                        // A string result is forwarded verbatim; re-encoding it as JSON
                        // would wrap it in quotes and escape every newline, wasting
                        // tokens and hurting readability. Non-string values are rendered
                        // as compact JSON.
                        Ok(v) => (
                            v.as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| v.to_string()),
                            false,
                        ),
                        Err(e) => (e.to_string(), true),
                    };

                    events
                        .emit(AgentEvent::ToolComplete(ToolCompleteEvent {
                            message_id: result_msg_id.clone(),
                            tool_call_id: tool_call_id.clone(),
                            output: output.clone(),
                            is_error,
                        }))
                        .await?;

                    Ok::<Message, AgentError>(Message {
                        id: result_msg_id,
                        role: Role::Tool,
                        parts: vec![ContentPart::ToolResult(ToolResultPart {
                            tool_call_id: tool_call_id.clone(),
                            output,
                            is_error,
                        })],
                    })
                });

            let results = futures_util::future::join_all(executions).await;
            for result in results {
                self.history.push(result?);
            }
        }
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
    use crate::{
        error::ToolCallError,
        events::EventSink,
        provider::{CompletionResponse, StopReason, ToolChoice},
        testkit::{CollectingEventSink, MockProvider, MockToolbox},
        tool::{EmptyToolbox, ToolSpec, Toolbox},
    };
    use async_trait::async_trait;
    use horsie_models::agent::{ContentPart, TextPart, ToolCallPart, Usage};
    use horsie_models::events::AgentEvent;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    // --- tests ---

    #[tokio::test]
    async fn test_simple_text_completion() {
        let mut agent = Agent::builder(MockProvider::text("Hello, world!"), Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let output = agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Completed(CompletedOutput { text }) => assert_eq!(text, "Hello, world!"),
            other => panic!(
                "expected Completed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert_eq!(output.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn test_completion_emits_message_complete_events() {
        let mut agent = Agent::builder(MockProvider::text("done"), Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            !sink.message_complete_ids().is_empty(),
            "expected at least 1 MessageComplete event, got 0"
        );
    }

    #[tokio::test]
    async fn test_input_message_event_emitted() {
        let mut agent = Agent::builder(MockProvider::text("ok"), Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "x"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let ie = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::InputMessage(ie) => Some(ie),
                _ => None,
            })
            .unwrap();
        assert_eq!(ie.message_id, "msg-1");
        assert!(matches!(ie.input, AgentInput::UserMessage(_)));
    }

    #[tokio::test]
    async fn test_run_complete_event_emitted() {
        let mut agent = Agent::builder(MockProvider::text("ok"), Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "x"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let count = sink
            .events()
            .iter()
            .filter(|e| matches!(e, AgentEvent::RunComplete(_)))
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_tool_call_cycle() {
        let provider =
            MockProvider::tool_then_text("tc1", "search", json!({"q": "rust"}), "found it");
        let toolbox = MockToolbox::echo("search");
        let mut agent = Agent::builder(provider, toolbox).build().unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::user_message("msg-1", "search rust"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Completed(CompletedOutput { text }) => assert_eq!(text, "found it"),
            other => panic!(
                "expected Completed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolExecuting(_)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolComplete(_)))
        );
    }

    #[tokio::test]
    async fn test_tool_result_message_id_is_derived_from_tool_call_id() {
        let provider = MockProvider::tool_then_text("tc1", "calc", json!({"x": 1}), "result: 1");
        let toolbox = MockToolbox::echo("calc");
        let mut agent = Agent::builder(provider, toolbox).build().unwrap();
        let sink = CollectingEventSink::new();

        agent
            .run(
                AgentInput::user_message("msg-1", "calc"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let tc = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::ToolComplete(tc) => Some(tc),
                _ => None,
            })
            .unwrap();
        assert_eq!(tc.message_id, format!("result:{}", tc.tool_call_id));
    }

    #[tokio::test]
    async fn test_handoff_tool_returns_handoff_result() {
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "hc1".to_string(),
                name: "handoff".to_string(),
                input: json!({"answer": 42}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }]);
        // The handoff tool must be advertised in the toolbox.
        let mut agent = Agent::builder(provider, MockToolbox::echo("handoff"))
            .with_handoff_tool("handoff")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Handoff(HandoffOutput { tool_name, data }) => {
                assert_eq!(tool_name, "handoff");
                assert_eq!(data["answer"], 42);
            }
            other => panic!("expected Handoff, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn test_build_fails_when_handoff_tool_missing() {
        let agent = Agent::builder(MockProvider::text("x"), Arc::new(EmptyToolbox))
            .with_handoff_tool("nope")
            .build();
        assert!(matches!(
            agent,
            Err(AgentBuildError::HandoffToolNotRegistered { .. })
        ));
    }

    #[tokio::test]
    async fn test_handoff_schema_validation_retries_then_succeeds() {
        // First call: invalid handoff input (missing required field). Second: valid.
        let toolbox = {
            let spec = ToolSpec {
                name: "finish".to_string(),
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["answer"],
                    "properties": { "answer": { "type": "number" } }
                }),
            };
            MockToolbox::new(vec![spec], Arc::new(|_, input| Ok(input)))
        };
        let provider = MockProvider::new(vec![
            CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "h1".into(),
                    name: "finish".into(),
                    input: json!({"wrong": true}),
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "h2".into(),
                    name: "finish".into(),
                    input: json!({"answer": 7}),
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
        ]);
        let mut agent = Agent::builder(provider, toolbox)
            .with_handoff_tool("finish")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let output = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match output.result {
            AgentResult::Handoff(HandoffOutput { data, .. }) => assert_eq!(data["answer"], 7),
            other => panic!("expected Handoff, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn test_handoff_validation_fails_after_max_retries() {
        let toolbox = {
            let spec = ToolSpec {
                name: "finish".to_string(),
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["answer"],
                    "properties": { "answer": { "type": "number" } }
                }),
            };
            MockToolbox::new(vec![spec], Arc::new(|_, input| Ok(input)))
        };
        // Always returns invalid input.
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "h".into(),
                name: "finish".into(),
                input: json!({"wrong": true}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
        }]);
        let config = AgentConfig {
            handoff_max_retries: 1,
            ..AgentConfig::default()
        };
        let mut agent = Agent::builder(provider, toolbox)
            .with_handoff_tool("finish")
            .with_config(config)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let err = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::HandoffValidationFailed { .. }));
    }

    #[tokio::test]
    async fn test_resume_with_tool_result() {
        let history = vec![
            Message {
                id: "m0".into(),
                role: Role::User,
                parts: vec![ContentPart::Text(TextPart {
                    text: "question".into(),
                })],
            },
            Message {
                id: "m1".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "hc1".into(),
                    name: "handoff".into(),
                    input: json!({}),
                })],
            },
        ];
        let provider = MockProvider::text("thanks for the answer");
        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
            .with_history(history)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::tool_result("hc1", "42", false),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(matches!(output.result, AgentResult::Completed(_)));

        let ie = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::InputMessage(ie) => Some(ie),
                _ => None,
            })
            .unwrap();
        assert_eq!(ie.message_id, "result:hc1");
        assert!(matches!(ie.input, AgentInput::ToolResult(_)));
    }

    #[tokio::test]
    async fn test_max_iterations_exceeded() {
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "t1".into(),
                name: "loop_tool".into(),
                input: json!({}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 2,
            },
        }]);
        let toolbox = MockToolbox::echo("loop_tool");
        let config = AgentConfig {
            max_iterations: 3,
            stuck_threshold: 10,
            nudge_threshold: 8,
            max_tokens: None,
            ..AgentConfig::default()
        };
        let mut agent = Agent::builder(provider, toolbox)
            .with_config(config)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let err = agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::MaxIterationsExceeded { max: 3 }));
    }

    #[tokio::test]
    async fn test_stuck_detection() {
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "s1".into(),
                name: "stuck_tool".into(),
                input: json!({"x": 1}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 2,
            },
        }]);
        let toolbox = MockToolbox::echo("stuck_tool");
        let config = AgentConfig {
            max_iterations: 20,
            stuck_threshold: 3,
            nudge_threshold: 2,
            max_tokens: None,
            ..AgentConfig::default()
        };
        let mut agent = Agent::builder(provider, toolbox)
            .with_config(config)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let err = agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::StuckInLoop { .. }));
    }

    #[tokio::test]
    async fn test_cancellation() {
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "c1".into(),
                name: "some_tool".into(),
                input: json!({}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 2,
            },
        }]);
        let toolbox = MockToolbox::echo("some_tool");
        let mut agent = Agent::builder(provider, toolbox).build().unwrap();
        let sink = CollectingEventSink::new();
        let token = CancellationToken::new();
        token.cancel();

        let err = agent
            .run(AgentInput::user_message("msg-1", "go"), &sink, token)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Cancelled));
    }

    /// Records the `tool_choice` of the first provider call.
    struct RecordingProvider {
        seen: Mutex<Option<ToolChoice>>,
        response: CompletionResponse,
    }

    #[async_trait]
    impl crate::provider::LlmProvider for RecordingProvider {
        fn model_id(&self) -> &str {
            "recording"
        }

        async fn complete(
            &self,
            request: crate::provider::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn EventSink,
        ) -> Result<CompletionResponse, crate::error::LlmError> {
            *self.seen.lock().unwrap() = Some(request.tool_choice.clone());
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn test_handoff_agent_forces_tool_choice_any() {
        // A handoff agent concludes immediately; assert the call used tool_choice=Any.
        let provider = Arc::new(RecordingProvider {
            seen: Mutex::new(None),
            response: CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "h1".into(),
                    name: "finish".into(),
                    input: json!({}),
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
        });
        let mut agent = Agent::builder(provider.clone(), MockToolbox::echo("finish"))
            .with_handoff_tool("finish")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            provider.seen.lock().unwrap().clone(),
            Some(ToolChoice::Any)
        ));
    }

    #[tokio::test]
    async fn test_handoff_agent_errors_on_plain_text_completion() {
        // Provider ignores tool_choice and keeps returning plain text. A handoff
        // agent must not silently complete — it nudges then fails.
        let provider = MockProvider::text("just chatting, no tool");
        let config = AgentConfig {
            handoff_max_retries: 1,
            ..AgentConfig::default()
        };
        let mut agent = Agent::builder(provider, MockToolbox::echo("finish"))
            .with_handoff_tool("finish")
            .with_config(config)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let err = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::HandoffValidationFailed { .. }));
    }

    #[tokio::test]
    async fn test_non_handoff_agent_uses_tool_choice_auto() {
        let provider = Arc::new(RecordingProvider {
            seen: Mutex::new(None),
            response: CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
        });
        let mut agent = Agent::builder(provider.clone(), Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            provider.seen.lock().unwrap().clone(),
            Some(ToolChoice::Auto)
        ));
    }

    #[tokio::test]
    async fn test_optional_handoff_tool_uses_tool_choice_auto() {
        // An *optional* handoff tool must not force tool_choice=Any, even though
        // one is registered — the model stays free to respond with plain text.
        let provider = Arc::new(RecordingProvider {
            seen: Mutex::new(None),
            response: CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "just chatting, no tool".into(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
        });
        let mut agent = Agent::builder(provider.clone(), MockToolbox::echo("finish"))
            .with_handoff_tool_optional("finish")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            provider.seen.lock().unwrap().clone(),
            Some(ToolChoice::Auto)
        ));
    }

    #[tokio::test]
    async fn test_optional_handoff_tool_accepts_plain_text_completion() {
        // Unlike a forced handoff tool, an optional one never nudges/rejects a
        // text-only turn — it's accepted immediately as `Completed`.
        let provider = MockProvider::text("just chatting, no tool");
        let mut agent = Agent::builder(provider, MockToolbox::echo("finish"))
            .with_handoff_tool_optional("finish")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let output = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            output.result,
            AgentResult::Completed(CompletedOutput { text }) if text == "just chatting, no tool"
        ));
    }

    #[tokio::test]
    async fn test_optional_handoff_tool_still_recognized_when_voluntarily_called() {
        // A voluntary call to the optional handoff tool is still interpreted as a
        // Handoff (not executed as a regular tool), exactly as a forced one would be.
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "h1".to_string(),
                name: "finish".to_string(),
                input: json!({"answer": 42}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
        }]);
        let mut agent = Agent::builder(provider, MockToolbox::echo("finish"))
            .with_handoff_tool_optional("finish")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let output = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match output.result {
            AgentResult::Handoff(HandoffOutput { tool_name, data }) => {
                assert_eq!(tool_name, "finish");
                assert_eq!(data["answer"], 42);
            }
            other => panic!("expected Handoff, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn test_string_tool_output_is_not_json_escaped() {
        // A tool returning a JSON string should reach the conversation verbatim —
        // no surrounding quotes, no `\n` escapes from a second JSON encoding.
        let provider = MockProvider::tool_then_text("t1", "cat", json!({}), "done");
        let toolbox = MockToolbox::new(
            vec![ToolSpec {
                name: "cat".to_string(),
                description: "cat".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            Arc::new(|_, _| Ok(Value::String("line1\nline2".to_string()))),
        );
        let mut agent = Agent::builder(provider, toolbox).build().unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let tc = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::ToolComplete(tc) => Some(tc),
                _ => None,
            })
            .unwrap();
        assert_eq!(tc.output, "line1\nline2");
        assert!(
            !tc.output.contains("\\n"),
            "output was JSON-escaped: {}",
            tc.output
        );
    }

    struct BarrierToolbox {
        barrier: Arc<tokio::sync::Barrier>,
        timed_out: Arc<std::sync::atomic::AtomicBool>,
        spec: ToolSpec,
    }

    #[async_trait]
    impl Toolbox for BarrierToolbox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![self.spec.clone()]
        }

        async fn execute(&self, _name: &str, input: Value) -> Result<Value, ToolCallError> {
            // Concurrent execution => both calls reach the barrier and proceed at
            // once. Sequential execution => the first call blocks here until the
            // timeout fires, flagging the regression.
            if tokio::time::timeout(std::time::Duration::from_secs(2), self.barrier.wait())
                .await
                .is_err()
            {
                self.timed_out
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(input)
        }
    }

    #[tokio::test]
    async fn test_tool_calls_in_a_turn_run_concurrently() {
        let provider = MockProvider::new(vec![
            CompletionResponse {
                parts: vec![
                    ContentPart::ToolCall(ToolCallPart {
                        id: "a".into(),
                        name: "wait".into(),
                        input: json!({}),
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "b".into(),
                        name: "wait".into(),
                        input: json!({}),
                    }),
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
        ]);
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let toolbox = Arc::new(BarrierToolbox {
            barrier: Arc::new(tokio::sync::Barrier::new(2)),
            timed_out: timed_out.clone(),
            spec: ToolSpec {
                name: "wait".to_string(),
                description: "wait".to_string(),
                input_schema: json!({ "type": "object" }),
            },
        });
        let mut agent = Agent::builder(provider, toolbox).build().unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !timed_out.load(std::sync::atomic::Ordering::SeqCst),
            "tool calls in a turn ran sequentially, not concurrently"
        );
    }

    #[tokio::test]
    async fn test_max_tokens_truncation_is_an_error_not_a_completion() {
        // stop_reason was computed but never read in production: a response cut
        // off by max_tokens looked exactly like a normal end of turn, so the
        // caller received a silently truncated answer as a success.
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: "half an ans".into(),
            })],
            stop_reason: StopReason::MaxTokens,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }]);
        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let err = agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .expect_err("truncation must not be reported as a completed turn");

        assert!(
            matches!(err, AgentError::Truncated { .. }),
            "expected Truncated, got {err:?}",
        );
    }
}
