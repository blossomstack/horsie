use crate::context::{AgentRuntimeContext, CONCLUDE_TOOL};
use crate::workflow_actor::WorkflowCommand;
use actor::{ActorContext, CommandEffect, EventSourcedActor, PersistenceId};
use agentcore::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentResult, ContentPart, EventSink,
    LlmProvider, Message, Toolbox, Usage,
};
use async_trait::async_trait;
use models::workflow::WorkflowAgentDef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Per-agent configuration distilled from a [`WorkflowAgentDef`]. Runtime only.
#[derive(Clone)]
pub struct AgentParams {
    pub system_prompt: Option<String>,
    /// Whether the agent produces structured output via `conclude`.
    pub has_output_schema: bool,
    /// Whether the agent may pause to ask the user.
    pub allow_ask_user: bool,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
}

impl AgentParams {
    pub fn from_def(def: &WorkflowAgentDef) -> Self {
        Self {
            system_prompt: def.system_prompt.clone(),
            has_output_schema: def.output_schema.is_some(),
            allow_ask_user: def.allow_ask_user,
            max_iterations: def.max_iterations,
            max_retries: def.max_retries.unwrap_or(0),
        }
    }

    /// The agent's handoff tool — the synthesized `conclude` tool when it has an
    /// output schema and/or may ask, else `None` (the agent ends with plain text).
    fn handoff_tool(&self) -> Option<String> {
        if self.has_output_schema || self.allow_ask_user {
            Some(CONCLUDE_TOOL.to_string())
        } else {
            None
        }
    }
}

/// Commands accepted by an [`AgentActor`].
pub enum AgentCommand {
    /// Begin a turn with fresh user input.
    Run { input: String },
    /// Resume a paused agent, supplying the user's reply as the pending tool result.
    InjectToolResult {
        tool_call_id: String,
        content: String,
    },
    /// Cancel an in-flight run.
    Cancel,
    /// Internal: a background run finished. Boxed to keep the command enum small.
    RunFinished(Box<RunReport>),
}

/// Coarse events that alter persisted agent state. Streaming observation events
/// (text/tool-input deltas) are emitted to the event sink but never journaled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentDomainEvent {
    InputMessage {
        message: Message,
    },
    MessageComplete {
        message: Message,
    },
    ToolComplete {
        tool_call_id: String,
        output: String,
        is_error: bool,
    },
    RunComplete {
        usage: Usage,
        iterations: u32,
    },
    RunCancelled,
}

/// The conversation history reconstructed by folding [`AgentDomainEvent`]s.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    pub messages: Vec<Message>,
}

/// Result of a background run, sent back to the actor as [`AgentCommand::RunFinished`].
pub struct RunReport {
    events: Vec<AgentDomainEvent>,
    outcome: RunOutcome,
}

enum RunOutcome {
    /// Agent ended its turn with plain text (no `conclude` tool registered).
    Completed {
        text: String,
    },
    /// Agent called the `conclude` tool; `data` is its raw input.
    Concluded {
        data: Value,
        tool_call_id: Option<String>,
    },
    Cancelled,
    Failed {
        error: String,
        recoverable: bool,
    },
}

/// An agent run, modelled as an event-sourced actor. Each `Run`/`InjectToolResult`
/// drives a background `agentcore::Agent` loop; the coarse outcome is journaled so
/// a crashed session recovers its conversation and continues.
pub struct AgentActor {
    ctx: AgentRuntimeContext,
    params: AgentParams,
    running: Option<CancellationToken>,
}

impl AgentActor {
    pub fn new(ctx: AgentRuntimeContext, params: AgentParams) -> Self {
        Self {
            ctx,
            params,
            running: None,
        }
    }

    /// The journal identity of an agent session: kind `"agent"`, id = the session
    /// UUID. Centralizes the kind so the workflow (e.g. fork) and the actor agree.
    pub fn persistence_id_for(session_id: uuid::Uuid) -> PersistenceId {
        PersistenceId::new("agent", session_id.to_string())
    }

    fn start_run(&mut self, input: AgentInput, ctx: &ActorContext<Self>, history: Vec<Message>) {
        let cancel = CancellationToken::new();
        self.running = Some(cancel.clone());

        let provider = self.ctx.provider.clone();
        let toolbox = self.ctx.toolbox.clone();
        let sink = self.ctx.event_sink.clone();
        let system_prompt = self.params.system_prompt.clone().unwrap_or_default();
        let handoff_tool = self.params.handoff_tool();
        let max_iterations = self.params.max_iterations;
        let max_retries = self.params.max_retries;
        let self_ref = ctx.self_ref();

        tokio::spawn(async move {
            let report = run_with_retries(
                provider,
                toolbox,
                sink,
                system_prompt,
                handoff_tool,
                max_iterations,
                max_retries,
                history,
                input,
                cancel,
            )
            .await;
            let _ = self_ref
                .tell(AgentCommand::RunFinished(Box::new(report)))
                .await;
        });
    }

    /// Interpret a `conclude` payload (or plain-text completion) and notify the
    /// parent workflow accordingly.
    async fn handle_finished(&mut self, report: RunReport) -> CommandEffect<AgentDomainEvent> {
        self.running = None;
        let session_id = self.ctx.session_id;
        let parent = self.ctx.parent_ref.clone();

        match report.outcome {
            RunOutcome::Completed { text } => {
                // No conclude tool: treat the final text as the output.
                let _ = parent
                    .tell(WorkflowCommand::AgentConcluded {
                        session_id,
                        output: Value::String(text),
                    })
                    .await;
                CommandEffect::PersistAndStop(report.events)
            }
            RunOutcome::Concluded { data, tool_call_id } => {
                match self.interpret(data, tool_call_id) {
                    Conclusion::Output(output) => {
                        let _ = parent
                            .tell(WorkflowCommand::AgentConcluded { session_id, output })
                            .await;
                        CommandEffect::PersistAndStop(report.events)
                    }
                    Conclusion::Ask {
                        tool_call_id,
                        question,
                    } => {
                        let _ = parent
                            .tell(WorkflowCommand::AgentAsked {
                                session_id,
                                tool_call_id,
                                question,
                            })
                            .await;
                        // Stay alive — InjectToolResult resumes this same session.
                        CommandEffect::PersistAndSnapshot(report.events)
                    }
                }
            }
            RunOutcome::Cancelled => {
                CommandEffect::PersistAndSnapshot(vec![AgentDomainEvent::RunCancelled])
            }
            RunOutcome::Failed { error, recoverable } => {
                let _ = parent
                    .tell(WorkflowCommand::AgentFailed {
                        session_id,
                        error,
                        recoverable,
                    })
                    .await;
                CommandEffect::Stop
            }
        }
    }

    /// Decide whether a `conclude` payload is a final output or an ask, based on
    /// the agent's configured variant.
    fn interpret(&self, data: Value, tool_call_id: Option<String>) -> Conclusion {
        let extract_question = |d: &Value| {
            d.get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match (self.params.has_output_schema, self.params.allow_ask_user) {
            // Kind-tagged union.
            (true, true) => {
                let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
                if kind == "ask" {
                    Conclusion::Ask {
                        tool_call_id,
                        question: extract_question(&data),
                    }
                } else {
                    Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null))
                }
            }
            // Output only: the payload is the output.
            (true, false) => Conclusion::Output(data),
            // Ask only: the payload is a question.
            (false, true) => Conclusion::Ask {
                tool_call_id,
                question: extract_question(&data),
            },
            // No conclude tool registered — shouldn't be reached via a handoff.
            (false, false) => Conclusion::Output(data),
        }
    }
}

enum Conclusion {
    Output(Value),
    Ask {
        tool_call_id: Option<String>,
        question: String,
    },
}

#[async_trait]
impl EventSourcedActor for AgentActor {
    type Command = AgentCommand;
    type Event = AgentDomainEvent;
    type State = AgentState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.ctx.session_id)
    }

    fn initial_state() -> AgentState {
        AgentState::default()
    }

    fn apply_event(mut state: AgentState, event: AgentDomainEvent) -> AgentState {
        match event {
            AgentDomainEvent::InputMessage { message }
            | AgentDomainEvent::MessageComplete { message } => state.messages.push(message),
            AgentDomainEvent::ToolComplete {
                tool_call_id,
                output,
                is_error,
            } => state
                .messages
                .push(Message::tool_result(tool_call_id, output, is_error)),
            AgentDomainEvent::RunComplete { .. } | AgentDomainEvent::RunCancelled => {}
        }
        state
    }

    async fn handle_command(
        &mut self,
        state: &AgentState,
        cmd: AgentCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            AgentCommand::Run { input } => {
                self.start_run(
                    AgentInput::user_message(new_message_id(), input),
                    ctx,
                    state.messages.clone(),
                );
                CommandEffect::None
            }
            AgentCommand::InjectToolResult {
                tool_call_id,
                content,
            } => {
                self.start_run(
                    AgentInput::tool_result(tool_call_id, content, false),
                    ctx,
                    state.messages.clone(),
                );
                CommandEffect::None
            }
            AgentCommand::Cancel => {
                if let Some(token) = &self.running {
                    token.cancel();
                }
                CommandEffect::None
            }
            AgentCommand::RunFinished(report) => self.handle_finished(*report).await,
        }
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Captures coarse agent events while forwarding every event to the real sink.
struct CapturingSink {
    inner: Arc<dyn EventSink>,
    captured: Mutex<Vec<AgentEvent>>,
}

impl CapturingSink {
    fn new(inner: Arc<dyn EventSink>) -> Self {
        Self {
            inner,
            captured: Mutex::new(Vec::new()),
        }
    }

    fn take(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut self.captured.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl EventSink for CapturingSink {
    fn emit(&self, event: AgentEvent) {
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(event.clone());
        }
        self.inner.emit(event);
    }
}

fn coarse_events(events: &[AgentEvent]) -> Vec<AgentDomainEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::InputMessage(ev) => Some(AgentDomainEvent::InputMessage {
                message: ev.input.to_message(),
            }),
            AgentEvent::MessageComplete(ev) => Some(AgentDomainEvent::MessageComplete {
                message: ev.message.clone(),
            }),
            AgentEvent::ToolComplete(ev) => Some(AgentDomainEvent::ToolComplete {
                tool_call_id: ev.tool_call_id.clone(),
                output: ev.output.clone(),
                is_error: ev.is_error,
            }),
            AgentEvent::RunComplete(ev) => Some(AgentDomainEvent::RunComplete {
                usage: ev.usage.clone(),
                iterations: ev.iterations,
            }),
            AgentEvent::MessageStart(_)
            | AgentEvent::MessageStop(_)
            | AgentEvent::TextChunk(_)
            | AgentEvent::ThinkingChunk(_)
            | AgentEvent::ToolCallStart(_)
            | AgentEvent::ToolCallInputDelta(_)
            | AgentEvent::ToolCallInputDone(_)
            | AgentEvent::ToolExecuting(_) => None,
        })
        .collect()
}

/// Find the tool-call id of the handoff tool by scanning captured assistant messages.
fn find_tool_call_id(events: &[AgentEvent], tool_name: &str) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        AgentEvent::MessageComplete(mc) => mc.message.parts.iter().find_map(|p| match p {
            ContentPart::ToolCall(tc) if tc.name == tool_name => Some(tc.id.clone()),
            ContentPart::ToolCall(_)
            | ContentPart::Text(_)
            | ContentPart::ToolResult(_)
            | ContentPart::Thinking(_) => None,
        }),
        AgentEvent::InputMessage(_)
        | AgentEvent::MessageStart(_)
        | AgentEvent::MessageStop(_)
        | AgentEvent::TextChunk(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ToolCallStart(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ToolCallInputDone(_)
        | AgentEvent::ToolExecuting(_)
        | AgentEvent::ToolComplete(_)
        | AgentEvent::RunComplete(_) => None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_with_retries(
    provider: Arc<dyn LlmProvider>,
    toolbox: Arc<dyn Toolbox>,
    sink: Arc<dyn EventSink>,
    system_prompt: String,
    handoff_tool: Option<String>,
    max_iterations: Option<u32>,
    max_retries: u32,
    history: Vec<Message>,
    input: AgentInput,
    cancel: CancellationToken,
) -> RunReport {
    let mut attempt: u32 = 0;
    loop {
        let capture = CapturingSink::new(sink.clone());
        let config = AgentConfig {
            max_iterations: max_iterations.unwrap_or_else(|| AgentConfig::default().max_iterations),
            ..AgentConfig::default()
        };
        let mut builder = Agent::builder(provider.clone(), toolbox.clone())
            .with_system_prompt(system_prompt.clone())
            .with_config(config)
            .with_history(history.clone());
        if let Some(name) = &handoff_tool {
            builder = builder.with_handoff_tool(name.clone());
        }

        let mut agent = match builder.build() {
            Ok(a) => a,
            Err(e) => {
                return RunReport {
                    events: Vec::new(),
                    outcome: RunOutcome::Failed {
                        error: e.to_string(),
                        recoverable: false,
                    },
                };
            }
        };

        let result = agent.run(input.clone(), &capture, cancel.clone()).await;
        let captured = capture.take();

        match result {
            Ok(output) => {
                let events = coarse_events(&captured);
                let outcome = match output.result {
                    AgentResult::Completed(c) => RunOutcome::Completed { text: c.text },
                    AgentResult::Handoff(h) => {
                        let tool_call_id = find_tool_call_id(&captured, &h.tool_name);
                        RunOutcome::Concluded {
                            data: h.data,
                            tool_call_id,
                        }
                    }
                };
                return RunReport { events, outcome };
            }
            Err(AgentError::Cancelled) => {
                return RunReport {
                    events: Vec::new(),
                    outcome: RunOutcome::Cancelled,
                };
            }
            Err(AgentError::Provider(e)) if attempt < max_retries => {
                attempt += 1;
                let backoff = Duration::from_millis(50u64 * (1u64 << attempt.min(6)));
                tracing::warn!(error = %e, attempt, "provider error; retrying after backoff");
                tokio::time::sleep(backoff).await;
                continue;
            }
            Err(AgentError::Provider(e)) => {
                return RunReport {
                    events: Vec::new(),
                    outcome: RunOutcome::Failed {
                        error: e.to_string(),
                        recoverable: true,
                    },
                };
            }
            Err(e) => {
                return RunReport {
                    events: Vec::new(),
                    outcome: RunOutcome::Failed {
                        error: e.to_string(),
                        recoverable: false,
                    },
                };
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
    use models::agent::{Role, TextPart, ToolCallPart, ToolResultPart};

    fn user_msg(text: &str) -> Message {
        Message {
            id: "u".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
        }
    }

    #[test]
    fn apply_event_rebuilds_history_in_order() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hello"),
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::MessageComplete {
                message: Message {
                    id: "a".into(),
                    role: Role::Assistant,
                    parts: vec![ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "search".into(),
                        input: serde_json::json!({}),
                    })],
                },
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::ToolComplete {
                tool_call_id: "tc1".into(),
                output: "result".into(),
                is_error: false,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                iterations: 1,
            },
        );

        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0].role, Role::User);
        assert_eq!(state.messages[1].role, Role::Assistant);
        assert_eq!(state.messages[2].role, Role::Tool);
        match &state.messages[2].parts[0] {
            ContentPart::ToolResult(ToolResultPart {
                tool_call_id,
                output,
                ..
            }) => {
                assert_eq!(tool_call_id, "tc1");
                assert_eq!(output, "result");
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn run_cancelled_is_noop_on_state() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hi"),
            },
        );
        let before = state.messages.len();
        state = AgentActor::apply_event(state, AgentDomainEvent::RunCancelled);
        assert_eq!(state.messages.len(), before);
    }

    #[test]
    fn coarse_events_filter_streaming_noise() {
        use models::events::{InputMessageEvent, TextChunkEvent};
        let events = vec![
            AgentEvent::InputMessage(InputMessageEvent {
                message_id: "m".into(),
                input: AgentInput::user_message("m", "hi"),
            }),
            AgentEvent::TextChunk(TextChunkEvent {
                message_id: "m".into(),
                index: 0,
                text: "noise".into(),
            }),
        ];
        let coarse = coarse_events(&events);
        assert_eq!(coarse.len(), 1);
        assert!(matches!(coarse[0], AgentDomainEvent::InputMessage { .. }));
    }
}
