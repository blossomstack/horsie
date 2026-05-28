use crate::{
    error::{AgentBuildError, AgentError},
    events::EventSink,
    provider::{CompletionRequest, LlmProvider, ToolChoice},
    tool::Toolbox,
};
use models::agent::{
    AgentInput, AgentOutput, AgentResult, CompletedOutput, ContentPart, HandoffOutput, Message,
    Role, ToolResultPart, Usage,
};
use models::events::{
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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 100,
            stuck_threshold: 5,
            nudge_threshold: 3,
            max_tokens: None,
        }
    }
}

pub struct Agent {
    pub(crate) provider: Arc<dyn LlmProvider>,
    pub(crate) system_prompt: String,
    pub(crate) toolbox: Arc<dyn Toolbox>,
    pub(crate) handoff_tool: Option<String>,
    pub(crate) config: AgentConfig,
    pub(crate) history: Vec<Message>,
}

pub struct AgentBuilder {
    provider: Arc<dyn LlmProvider>,
    system_prompt: String,
    toolbox: Arc<dyn Toolbox>,
    handoff_tool: Option<String>,
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
            config: AgentConfig::default(),
            history: Vec::new(),
        }
    }

    pub fn with_system_prompt(mut self, p: impl Into<String>) -> Self {
        self.system_prompt = p.into();
        self
    }

    pub fn with_handoff_tool(mut self, n: impl Into<String>) -> Self {
        self.handoff_tool = Some(n.into());
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
        Ok(Agent {
            provider: self.provider,
            system_prompt: self.system_prompt,
            toolbox: self.toolbox,
            handoff_tool: self.handoff_tool,
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

    pub async fn run(
        &mut self,
        input: AgentInput,
        events: &dyn EventSink,
        cancel: CancellationToken,
    ) -> Result<AgentOutput, AgentError> {
        let run_id = Uuid::new_v4().to_string();

        let input_msg = input.to_message();
        events.emit(AgentEvent::InputMessage(InputMessageEvent {
            message_id: input.message_id(),
            input,
        }));
        self.history.push(input_msg);

        let mut total_usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        let mut iteration: u32 = 0;
        let mut recent_fingerprints: VecDeque<String> = VecDeque::new();

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
                tool_choice: ToolChoice::Auto,
                max_tokens: self.config.max_tokens,
            };

            let msg_id = Uuid::new_v4().to_string();
            events.emit(AgentEvent::MessageStart(MessageStartEvent {
                message_id: msg_id.clone(),
                role: Role::Assistant,
            }));

            let response = self
                .provider
                .complete(request, &msg_id, events)
                .await
                .map_err(AgentError::Provider)?;

            events.emit(AgentEvent::MessageStop(MessageStopEvent {
                message_id: msg_id.clone(),
            }));

            total_usage.input_tokens += response.usage.input_tokens;
            total_usage.output_tokens += response.usage.output_tokens;

            let assistant_msg = Message {
                id: msg_id.clone(),
                role: Role::Assistant,
                parts: response.parts.clone(),
            };
            events.emit(AgentEvent::MessageComplete(MessageCompleteEvent {
                message_id: msg_id,
                message: assistant_msg.clone(),
            }));
            self.history.push(assistant_msg);

            let tool_calls = extract_tool_calls(&response.parts);

            if tool_calls.is_empty() {
                events.emit(AgentEvent::RunComplete(RunCompleteEvent {
                    message_id: run_id.clone(),
                    usage: total_usage.clone(),
                    iterations: iteration,
                }));
                return Ok(AgentOutput {
                    result: AgentResult::Completed(CompletedOutput {
                        text: extract_text(&response.parts),
                    }),
                    usage: total_usage,
                });
            }

            if let Some(ref handoff_name) = self.handoff_tool
                && let Some((_, name, data)) = tool_calls.iter().find(|(_, n, _)| n == handoff_name)
            {
                events.emit(AgentEvent::RunComplete(RunCompleteEvent {
                    message_id: run_id.clone(),
                    usage: total_usage.clone(),
                    iterations: iteration,
                }));
                return Ok(AgentOutput {
                    result: AgentResult::Handoff(HandoffOutput {
                        tool_name: name.clone(),
                        data: data.clone(),
                    }),
                    usage: total_usage,
                });
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

            for (tool_call_id, name, input) in &tool_calls {
                let result_msg_id = format!("result:{tool_call_id}");

                events.emit(AgentEvent::ToolExecuting(ToolExecutingEvent {
                    message_id: result_msg_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                }));

                let (output, is_error) = match self.toolbox.execute(name, input.clone()).await {
                    Ok(v) => (v.to_string(), false),
                    Err(e) => (e.to_string(), true),
                };

                events.emit(AgentEvent::ToolComplete(ToolCompleteEvent {
                    message_id: result_msg_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    output: output.clone(),
                    is_error,
                }));

                self.history.push(Message {
                    id: result_msg_id,
                    role: Role::Tool,
                    parts: vec![ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: tool_call_id.clone(),
                        output,
                        is_error,
                    })],
                });
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
        provider::{CompletionResponse, StopReason},
        tool::{EmptyToolbox, ToolSpec, Toolbox},
    };
    use async_trait::async_trait;
    use models::agent::{ContentPart, TextPart, ToolCallPart, Usage};
    use models::events::AgentEvent;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    // --- support types ---

    struct MockProvider {
        responses: Vec<CompletionResponse>,
        call_index: Mutex<usize>,
    }

    impl MockProvider {
        fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses,
                call_index: Mutex::new(0),
            })
        }

        fn text(text: &str) -> Arc<Self> {
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

        fn tool_then_text(tool_id: &str, tool_name: &str, input: Value, reply: &str) -> Arc<Self> {
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
    impl crate::provider::LlmProvider for MockProvider {
        fn model_id(&self) -> &str {
            "mock-model"
        }

        async fn complete(
            &self,
            _request: crate::provider::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn EventSink,
        ) -> Result<CompletionResponse, crate::error::LlmError> {
            let mut idx = self.call_index.lock().unwrap();
            let response = self.responses[*idx % self.responses.len()].clone();
            *idx += 1;
            Ok(response)
        }
    }

    type ToolHandler = Arc<dyn Fn(&str, Value) -> Result<Value, ToolCallError> + Send + Sync>;

    struct MockToolbox {
        specs: Vec<ToolSpec>,
        handler: ToolHandler,
    }

    impl MockToolbox {
        fn echo(name: &str) -> Arc<Self> {
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

    struct CollectingEventSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl CollectingEventSink {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<AgentEvent> {
            self.events.lock().unwrap().clone()
        }

        fn message_complete_ids(&self) -> Vec<String> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::MessageComplete(mc) => Some(mc.message_id),
                    _ => None,
                })
                .collect()
        }
    }

    impl EventSink for CollectingEventSink {
        fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

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
        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
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
}
