use crate::context::{AgentRuntimeContext, CONCLUDE_TOOL};
use crate::workflow_actor::WorkflowCommand;
use actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use agentcore::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentResult, ContentPart, EventSink,
    EventSinkError, LlmProvider, Message, Role, Toolbox, Usage,
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
    /// Whether the agent may arm timers and park itself to await them.
    pub allow_timers: bool,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
}

impl AgentParams {
    pub fn from_def(def: &WorkflowAgentDef) -> Self {
        Self {
            system_prompt: def.system_prompt.clone(),
            has_output_schema: def.output_schema.is_some(),
            allow_ask_user: def.allow_ask_user,
            allow_timers: def.allow_timers.unwrap_or(false),
            max_iterations: def.max_iterations,
            max_retries: def.max_retries.unwrap_or(0),
        }
    }

    /// The agent's handoff tool — the synthesized `conclude` tool when it has an
    /// output schema, may ask, or may park on timers, else `None` (plain text end).
    fn handoff_tool(&self) -> Option<String> {
        if self.has_output_schema || self.allow_ask_user || self.allow_timers {
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
    /// Internal: coarse events captured mid-run. `ack` lets the emitting loop await
    /// the durable write before continuing, so persistence applies backpressure on
    /// the agent loop, and reports the write outcome so a journal failure aborts the
    /// run instead of proceeding on an unrecorded history. Persistence still flows
    /// through this one mailbox.
    PersistProgress {
        events: Vec<AgentDomainEvent>,
        ack: tokio::sync::oneshot::Sender<Result<(), actor::JournalError>>,
    },
    /// Internal: a background run finished. Boxed to keep the command enum small.
    RunFinished(Box<RunReport>),
    /// Arm a timer; replies with the new timer id once recorded.
    ArmTimer {
        label: String,
        kind: crate::timers::TimerKind,
        after_secs: u64,
        reply: tokio::sync::oneshot::Sender<crate::timers::TimerId>,
    },
    /// List active timers.
    ListTimers {
        reply: tokio::sync::oneshot::Sender<Vec<crate::timers::TimerView>>,
    },
    /// Cancel one or all timers; replies with the ids actually removed.
    CancelTimer {
        selector: crate::timers::CancelSelector,
        reply: tokio::sync::oneshot::Sender<Vec<crate::timers::TimerId>>,
    },
    /// Internal: a timer's sleep elapsed.
    TimerFired { id: crate::timers::TimerId },
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
    /// A timer was armed.
    TimerArmed {
        record: crate::timers::TimerRecord,
    },
    /// One or more timers were cancelled.
    TimerCancelled {
        ids: Vec<crate::timers::TimerId>,
    },
    /// A timer fired. `next_fire_at_unix_ms` carries the re-armed fire time for a
    /// recurring timer (so the fold stays pure); `None` removes a one-shot.
    TimerFired {
        id: crate::timers::TimerId,
        next_fire_at_unix_ms: Option<u64>,
    },
    /// The agent parked itself awaiting its timers.
    Parked,
}

/// The conversation history reconstructed by folding [`AgentDomainEvent`]s, plus
/// any timers the agent has armed and whether it is currently parked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    pub messages: Vec<Message>,
    /// Active timers — durable so they re-arm on recovery and back `list`/`cancel`.
    #[serde(default)]
    pub timers: Vec<crate::timers::TimerRecord>,
    /// True while the agent has parked itself awaiting a timer (no run in flight).
    #[serde(default)]
    pub parked: bool,
}

/// Result of a background run, sent back to the actor as [`AgentCommand::RunFinished`].
/// Coarse events are streamed separately and incrementally via
/// [`AgentCommand::PersistProgress`]; this carries only the terminal outcome.
pub struct RunReport {
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
/// drives a background `agentcore::Agent` loop; coarse events are journaled
/// incrementally so a crashed session recovers its conversation and continues.
pub struct AgentActor {
    ctx: AgentRuntimeContext,
    params: AgentParams,
    running: Option<CancellationToken>,
    /// A timer fired while a run was in flight; consume it when the run parks.
    pending_wake: bool,
}

impl AgentActor {
    pub fn new(ctx: AgentRuntimeContext, params: AgentParams) -> Self {
        Self {
            ctx,
            params,
            running: None,
            pending_wake: false,
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

        let self_ref = ctx.self_ref();
        let provider = self.ctx.provider.clone();
        // Timer-capable agents run with the timer control tools layered on; these
        // execute by `ask`ing this actor and are never sent to the sandboxed runtime.
        let toolbox: Arc<dyn Toolbox> = if self.params.allow_timers {
            Arc::new(TimerToolbox {
                inner: self.ctx.toolbox.clone(),
                actor: self_ref.clone(),
            })
        } else {
            self.ctx.toolbox.clone()
        };
        let inner_sink = self.ctx.event_sink.clone();
        let system_prompt = self.params.system_prompt.clone().unwrap_or_default();
        let handoff_tool = self.params.handoff_tool();
        let max_iterations = self.params.max_iterations;
        let max_retries = self.params.max_retries;

        tokio::spawn(async move {
            // The sink persists each coarse event by `ask`ing this actor and awaiting
            // the durable write, so the LLM loop has end-to-end backpressure:
            // `emit().await` does not return until the event is journaled. Persistence
            // still flows through the actor's single mailbox (`PersistProgress`),
            // never the journal directly.
            let sink: Arc<dyn EventSink> = Arc::new(PersistSink {
                inner: inner_sink,
                actor: self_ref.clone(),
            });
            let outcome = run_with_retries(
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
            // All coarse events were already persisted (each `emit` awaited its ack),
            // so `RunFinished` lands after them in mailbox order.
            let _ = self_ref
                .tell(AgentCommand::RunFinished(Box::new(RunReport { outcome })))
                .await;
        });
    }

    /// Interpret a `conclude` payload (or plain-text completion) and notify the
    /// parent workflow accordingly. The conversation events were already persisted
    /// incrementally via [`AgentCommand::PersistProgress`], so this only records the
    /// terminal transition and decides the actor's lifecycle.
    async fn handle_finished(
        &mut self,
        report: RunReport,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
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
                CommandEffect::stop()
            }
            RunOutcome::Concluded { data, tool_call_id } => {
                match self.interpret(data, tool_call_id) {
                    Conclusion::Output(output) => {
                        let _ = parent
                            .tell(WorkflowCommand::AgentConcluded { session_id, output })
                            .await;
                        CommandEffect::stop()
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
                        // Snapshot to compact the incrementally-persisted log.
                        CommandEffect::snapshot()
                    }
                    Conclusion::Park => self.park_or_resume(state, ctx, session_id, parent).await,
                }
            }
            RunOutcome::Cancelled => {
                // Snapshot to compact the incrementally-persisted log on cancel.
                CommandEffect::persist(vec![AgentDomainEvent::RunCancelled]).and_snapshot()
            }
            RunOutcome::Failed { error, recoverable } => {
                let _ = parent
                    .tell(WorkflowCommand::AgentFailed {
                        session_id,
                        error,
                        recoverable,
                    })
                    .await;
                // The partial conversation was already journaled incrementally, so the
                // failed session stays inspectable and a recoverable failure can
                // `resume`/`fork` from where it stopped.
                CommandEffect::stop()
            }
        }
    }

    /// Decide whether a `conclude` payload is a final output, an ask, or a park,
    /// based on the agent's configured variant.
    fn interpret(&self, data: Value, tool_call_id: Option<String>) -> Conclusion {
        classify_conclusion(
            self.params.has_output_schema,
            self.params.allow_ask_user,
            self.params.allow_timers,
            data,
            tool_call_id,
        )
    }

    /// Decide what a `park` conclusion means: an illegal park (no timers fails the
    /// run), an immediate resume (a timer fired during the run), or a real park
    /// (stay alive, status → Parked).
    async fn park_or_resume(
        &mut self,
        state: &AgentState,
        ctx: &ActorContext<Self>,
        session_id: uuid::Uuid,
        parent: ActorRef<WorkflowCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        if state.timers.is_empty() {
            let _ = parent
                .tell(WorkflowCommand::AgentFailed {
                    session_id,
                    error: "agent parked with no active timers — nothing would ever wake it"
                        .to_string(),
                    recoverable: false,
                })
                .await;
            return CommandEffect::stop();
        }
        if self.pending_wake {
            // A timer fired mid-run; go straight back to work instead of parking.
            self.pending_wake = false;
            let wake = AgentInput::user_message(
                new_message_id(),
                "A timer fired while you were busy — re-check now.".to_string(),
            );
            let input_event = AgentDomainEvent::InputMessage {
                message: wake.to_message(),
            };
            self.start_run(wake, ctx, state.messages.clone());
            return CommandEffect::persist(vec![input_event]);
        }
        let _ = parent
            .tell(WorkflowCommand::AgentParked { session_id })
            .await;
        CommandEffect::persist(vec![AgentDomainEvent::Parked]).and_snapshot()
    }

    /// A timer's sleep elapsed. Re-arm a recurring timer, then resume the agent with
    /// a wake message — unless a run is already in flight, in which case coalesce the
    /// wake and let the run consume it when it parks.
    async fn handle_timer_fired(
        &mut self,
        id: crate::timers::TimerId,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(record) = state.timers.iter().find(|t| t.id == id).cloned() else {
            // Cancelled or already removed — a stale sleep. Ignore.
            return CommandEffect::none();
        };
        let display_count = record.fire_count + 1;
        let now = crate::timers::now_unix_ms();
        // Re-arm recurring; remove one-shot.
        let next_fire_at_unix_ms = match record.kind {
            crate::timers::TimerKind::Recurring => {
                let next = now.saturating_add(record.interval_secs.saturating_mul(1000));
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(record.interval_secs),
                );
                Some(next)
            }
            crate::timers::TimerKind::OneShot => None,
        };
        let fired = AgentDomainEvent::TimerFired {
            id,
            next_fire_at_unix_ms,
        };

        if self.running.is_some() {
            // A run is in flight: record the fire (re-arm) and remember to wake when
            // the run parks. Multiple fires coalesce into one wake.
            self.pending_wake = true;
            return CommandEffect::persist(vec![fired]);
        }

        // Idle/parked: start a fresh run with the wake message.
        let wake = AgentInput::user_message(new_message_id(), record.wake_message(display_count));
        let input_event = AgentDomainEvent::InputMessage {
            message: wake.to_message(),
        };
        self.start_run(wake, ctx, state.messages.clone());
        CommandEffect::persist(vec![fired, input_event])
    }
}

/// Classify a `conclude` payload into the agent's terminal intent. With timers the
/// payload is always `kind`-tagged (`submit`/`park`/`ask`); without, it follows the
/// legacy (has_output, allow_ask) shape.
fn classify_conclusion(
    has_output_schema: bool,
    allow_ask_user: bool,
    allow_timers: bool,
    data: Value,
    tool_call_id: Option<String>,
) -> Conclusion {
    let extract_question = |d: &Value| {
        d.get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    if allow_timers {
        let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
        return match kind {
            "park" => Conclusion::Park,
            "ask" => Conclusion::Ask {
                tool_call_id,
                question: extract_question(&data),
            },
            _ => Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null)),
        };
    }
    match (has_output_schema, allow_ask_user) {
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

#[derive(Debug)]
enum Conclusion {
    Output(Value),
    Ask {
        tool_call_id: Option<String>,
        question: String,
    },
    Park,
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
            AgentDomainEvent::InputMessage { message } => {
                // A new turn began — the agent is no longer parked.
                state.parked = false;
                state.messages.push(message);
            }
            AgentDomainEvent::MessageComplete { message } => state.messages.push(message),
            AgentDomainEvent::ToolComplete {
                tool_call_id,
                output,
                is_error,
            } => state
                .messages
                .push(Message::tool_result(tool_call_id, output, is_error)),
            AgentDomainEvent::TimerArmed { record } => state.timers.push(record),
            AgentDomainEvent::TimerCancelled { ids } => {
                state.timers.retain(|t| !ids.contains(&t.id));
            }
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms,
            } => match next_fire_at_unix_ms {
                Some(next) => {
                    if let Some(t) = state.timers.iter_mut().find(|t| t.id == id) {
                        t.fire_at_unix_ms = next;
                        t.fire_count += 1;
                    }
                }
                None => state.timers.retain(|t| t.id != id),
            },
            AgentDomainEvent::Parked => state.parked = true,
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
                let agent_input = AgentInput::user_message(new_message_id(), input);
                // Persist the input message here (not via the streaming sink), so a
                // turn-restarting provider retry that re-emits it can never
                // double-persist it into two consecutive user messages.
                let input_event = AgentDomainEvent::InputMessage {
                    message: agent_input.to_message(),
                };
                self.start_run(agent_input, ctx, state.messages.clone());
                CommandEffect::persist(vec![input_event])
            }
            AgentCommand::InjectToolResult {
                tool_call_id,
                content,
            } => {
                let agent_input = AgentInput::tool_result(tool_call_id, content, false);
                let input_event = AgentDomainEvent::InputMessage {
                    message: agent_input.to_message(),
                };
                self.start_run(agent_input, ctx, state.messages.clone());
                CommandEffect::persist(vec![input_event])
            }
            AgentCommand::PersistProgress { events, ack } => {
                CommandEffect::persist(events).and_ack(ack)
            }
            AgentCommand::Cancel => {
                if let Some(token) = &self.running {
                    token.cancel();
                }
                CommandEffect::none()
            }
            AgentCommand::ArmTimer {
                label,
                kind,
                after_secs,
                reply,
            } => {
                let now = crate::timers::now_unix_ms();
                let record = crate::timers::TimerRecord::arm(
                    label,
                    kind,
                    std::time::Duration::from_secs(after_secs),
                    now,
                );
                let id = record.id.clone();
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(after_secs),
                );
                let _ = reply.send(id);
                CommandEffect::persist(vec![AgentDomainEvent::TimerArmed { record }])
            }
            AgentCommand::ListTimers { reply } => {
                let now = crate::timers::now_unix_ms();
                let views = state.timers.iter().map(|t| t.view(now)).collect();
                let _ = reply.send(views);
                CommandEffect::none()
            }
            AgentCommand::CancelTimer { selector, reply } => {
                let ids: Vec<crate::timers::TimerId> = match selector {
                    crate::timers::CancelSelector::All => {
                        state.timers.iter().map(|t| t.id.clone()).collect()
                    }
                    crate::timers::CancelSelector::One(id) => {
                        if state.timers.iter().any(|t| t.id == id) {
                            vec![id]
                        } else {
                            vec![]
                        }
                    }
                };
                let _ = reply.send(ids.clone());
                if ids.is_empty() {
                    CommandEffect::none()
                } else {
                    CommandEffect::persist(vec![AgentDomainEvent::TimerCancelled { ids }])
                }
            }
            AgentCommand::TimerFired { id } => self.handle_timer_fired(id, state, ctx).await,
            AgentCommand::RunFinished(report) => self.handle_finished(*report, state, ctx).await,
        }
    }

    /// After recovery, re-drive an interrupted session. An empty history means
    /// nothing ran yet (the workflow will send `Run`); otherwise the process died
    /// mid-turn, so sanitize any dangling tool calls and re-enter the loop with a
    /// synthetic continuation message. The synthetic input is intentionally not
    /// persisted as a new turn boundary: if we crash again before progress,
    /// recovery simply re-synthesizes it.
    async fn on_recovery_complete(&mut self, state: &AgentState, ctx: &mut ActorContext<Self>) {
        // Re-arm every surviving timer with its remaining delay (fires immediately if
        // already due). Do this whether parked or mid-run, so timers keep firing.
        let now = crate::timers::now_unix_ms();
        for t in &state.timers {
            spawn_timer_sleep(ctx.self_ref(), t.id.clone(), t.remaining(now));
        }
        // A parked agent waits for a timer — do not re-drive a turn.
        if state.parked {
            return;
        }
        if state.messages.is_empty() {
            return;
        }
        let history = sanitize_for_resume(state.messages.clone());
        self.start_run(
            AgentInput::user_message(new_message_id(), "continue the interrupted task"),
            ctx,
            history,
        );
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Spawn a one-shot sleep that tells the actor `TimerFired` after `delay`. The
/// firing is journaled/handled in the actor; a stale fire (timer since cancelled)
/// is ignored there, so an un-cancellable sleep task is harmless.
fn spawn_timer_sleep(
    self_ref: ActorRef<AgentCommand>,
    id: crate::timers::TimerId,
    delay: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = self_ref.tell(AgentCommand::TimerFired { id }).await;
    });
}

/// Wraps an agent's toolbox, adding the three timer control tools. They execute by
/// `ask`ing the owning [`AgentActor`] (never forwarded to the sandboxed runtime).
struct TimerToolbox {
    inner: Arc<dyn Toolbox>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TimerToolbox {
    fn specs(&self) -> Vec<agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(crate::timers::timer_tool_specs());
        specs
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, agentcore::ToolCallError> {
        use crate::timers::{CancelSelector, TimerId, TimerKind};
        use agentcore::ToolCallError;
        match name {
            "set_timer" => {
                let kind = match input.get("kind").and_then(Value::as_str) {
                    Some("one_shot") => TimerKind::OneShot,
                    Some("recurring") => TimerKind::Recurring,
                    _ => {
                        return Err(ToolCallError::InvalidInput(
                            "set_timer.kind must be 'one_shot' or 'recurring'".to_string(),
                        ));
                    }
                };
                let Some(after_secs) = input
                    .get("after_secs")
                    .and_then(Value::as_u64)
                    .filter(|n| *n >= 1)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.after_secs must be an integer >= 1".to_string(),
                    ));
                };
                let label = input
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let id = self
                    .actor
                    .ask(|reply| AgentCommand::ArmTimer {
                        label,
                        kind,
                        after_secs,
                        reply,
                    })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                Ok(serde_json::json!({ "timer_id": id.0 }))
            }
            "list_timers" => {
                let views = self
                    .actor
                    .ask(|reply| AgentCommand::ListTimers { reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                serde_json::to_value(views)
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))
            }
            "cancel_timer" => {
                let selector = if input.get("all").and_then(Value::as_bool) == Some(true) {
                    CancelSelector::All
                } else if let Some(id) = input.get("id").and_then(Value::as_str) {
                    CancelSelector::One(TimerId(id.to_string()))
                } else {
                    return Err(ToolCallError::InvalidInput(
                        "cancel_timer requires 'id' or 'all': true".to_string(),
                    ));
                };
                let ids = self
                    .actor
                    .ask(|reply| AgentCommand::CancelTimer { selector, reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                let ids: Vec<String> = ids.into_iter().map(|i| i.0).collect();
                Ok(serde_json::json!({ "cancelled": ids }))
            }
            _ => self.inner.execute(name, input).await,
        }
    }
}

/// Captures coarse agent events while forwarding every event to the inner sink.
/// Used only inside [`run_with_retries`] to locate the handoff tool-call id;
/// persistence (with backpressure) happens in the inner [`PersistSink`].
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

#[async_trait]
impl EventSink for CapturingSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(event.clone());
        }
        // Propagate the inner sink's outcome so a durability failure aborts the run.
        self.inner.emit(event).await
    }
}

/// Persists each coarse domain event by `ask`ing the agent actor and awaiting the
/// durable write before returning — this is what gives the agent loop end-to-end
/// backpressure. Persistence flows through the actor's mailbox
/// ([`AgentCommand::PersistProgress`]), never the journal directly. Every event is
/// also forwarded to the inner observation sink.
///
/// `InputMessage` is intentionally NOT persisted here: the actor persists the input
/// itself when handling `Run`/`InjectToolResult`, so a turn-restarting retry that
/// re-emits the input can never double-persist it into two consecutive user
/// messages.
struct PersistSink {
    inner: Arc<dyn EventSink>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl EventSink for PersistSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let Some(coarse) = coarse_event(&event) {
            // Await the durable write and act on its outcome:
            // - Ok(Ok(()))  → journaled; proceed.
            // - Ok(Err(je)) → the journal write FAILED. Abort the run rather than
            //   continue on a history that was never recorded.
            // - Err(_)      → the actor has stopped (the run is being torn down), so
            //   there is nothing to persist to and nothing to wait for; drop quietly.
            match self
                .actor
                .ask(|ack| AgentCommand::PersistProgress {
                    events: vec![coarse],
                    ack,
                })
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(je)) => {
                    return Err(EventSinkError(format!("journal write failed: {je}")));
                }
                Err(_actor_gone) => {}
            }
        }
        self.inner.emit(event).await
    }
}

/// Map a single streaming event to the coarse domain event that should be
/// persisted, or `None` for streaming noise and for `InputMessage` (see
/// [`PersistSink`]).
fn coarse_event(e: &AgentEvent) -> Option<AgentDomainEvent> {
    match e {
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
        AgentEvent::InputMessage(_)
        | AgentEvent::MessageStart(_)
        | AgentEvent::MessageStop(_)
        | AgentEvent::TextBlockStart(_)
        | AgentEvent::TextChunk(_)
        | AgentEvent::ThinkingBlockStart(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ThinkingSignatureChunk(_)
        | AgentEvent::ToolCallStart(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ContentBlockStop(_)
        | AgentEvent::ToolExecuting(_) => None,
    }
}

/// Make a recovered history well-formed for the provider: every `tool_use` in the
/// last assistant message must have a matching `tool_result`. Any missing one (an
/// interrupted tool call) gets a synthetic error result so the model can retry.
fn sanitize_for_resume(mut messages: Vec<Message>) -> Vec<Message> {
    let answered: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
            ContentPart::Text(_) | ContentPart::ToolCall(_) | ContentPart::Thinking(_) => None,
        })
        .collect();
    let dangling: Vec<String> = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolCall(tc) if !answered.contains(&tc.id) => Some(tc.id.clone()),
                    ContentPart::ToolCall(_)
                    | ContentPart::Text(_)
                    | ContentPart::ToolResult(_)
                    | ContentPart::Thinking(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for id in dangling {
        messages.push(Message::tool_result(
            id,
            "interrupted by shutdown, not completed",
            true,
        ));
    }
    messages
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
        | AgentEvent::TextBlockStart(_)
        | AgentEvent::TextChunk(_)
        | AgentEvent::ThinkingBlockStart(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ThinkingSignatureChunk(_)
        | AgentEvent::ToolCallStart(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ContentBlockStop(_)
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
) -> RunOutcome {
    let mut attempt: u32 = 0;
    loop {
        // CapturingSink wraps the PersistSink: it records events only to locate the
        // handoff tool-call id; persistence (with backpressure) happens in PersistSink.
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
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: false,
                };
            }
        };

        let result = agent.run(input.clone(), &capture, cancel.clone()).await;
        let captured = capture.take();

        match result {
            Ok(output) => {
                return match output.result {
                    AgentResult::Completed(c) => RunOutcome::Completed { text: c.text },
                    AgentResult::Handoff(h) => {
                        let tool_call_id = find_tool_call_id(&captured, &h.tool_name);
                        RunOutcome::Concluded {
                            data: h.data,
                            tool_call_id,
                        }
                    }
                };
            }
            Err(AgentError::Cancelled) => return RunOutcome::Cancelled,
            Err(AgentError::Provider(e)) if attempt < max_retries => {
                attempt += 1;
                let backoff = Duration::from_millis(50u64 * (1u64 << attempt.min(6)));
                tracing::warn!(error = %e, attempt, "provider error; retrying after backoff");
                tokio::time::sleep(backoff).await;
                continue;
            }
            Err(AgentError::Provider(e)) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: true,
                };
            }
            Err(e) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: false,
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
    use models::agent::{TextPart, ToolCallPart, ToolResultPart};

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
    fn sanitize_appends_error_results_for_dangling_tool_calls() {
        let history = vec![
            user_msg("do it"),
            Message {
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc2".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                ],
            },
            Message::tool_result("tc1", "ok", false),
        ];
        let fixed = sanitize_for_resume(history);
        // tc2 was dangling → an error tool_result is appended at the end.
        let last = fixed.last().unwrap();
        match &last.parts[0] {
            ContentPart::ToolResult(r) => {
                assert_eq!(r.tool_call_id, "tc2");
                assert!(r.is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_leaves_well_formed_history_untouched() {
        let history = vec![
            user_msg("do it"),
            Message {
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "tc1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                })],
            },
            Message::tool_result("tc1", "ok", false),
        ];
        let before = history.len();
        let fixed = sanitize_for_resume(history);
        assert_eq!(fixed.len(), before);
    }

    #[test]
    fn classify_park_kind_when_timers_enabled() {
        use serde_json::json;
        // timers on: a kind=park payload classifies as Park.
        let c = classify_conclusion(true, true, true, json!({"kind": "park"}), None);
        assert!(matches!(c, Conclusion::Park));
        // kind=submit classifies as Output(output field).
        let c = classify_conclusion(
            true,
            true,
            true,
            json!({"kind": "submit", "output": {"x": 1}}),
            None,
        );
        match c {
            Conclusion::Output(v) => assert_eq!(v["x"], 1),
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn timer_events_fold_into_state() {
        use crate::timers::{TimerKind, TimerRecord};
        use std::time::Duration;

        let rec = TimerRecord::arm(
            "pr".into(),
            TimerKind::Recurring,
            Duration::from_secs(60),
            0,
        );
        let id = rec.id.clone();
        let mut state = AgentActor::initial_state();

        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: rec });
        assert_eq!(state.timers.len(), 1);

        // Recurring fire re-arms in place with a carried next fire time and bumped count.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired {
                id: id.clone(),
                next_fire_at_unix_ms: Some(120_000),
            },
        );
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.timers[0].fire_count, 1);
        assert_eq!(state.timers[0].fire_at_unix_ms, 120_000);

        // One-shot fire (None) removes it.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms: None,
            },
        );
        assert!(state.timers.is_empty());
    }

    #[test]
    fn park_sets_parked_and_input_clears_it() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, AgentDomainEvent::Parked);
        assert!(state.parked);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("wake"),
            },
        );
        assert!(!state.parked);
    }

    #[test]
    fn cancel_event_removes_selected_timers() {
        use crate::timers::{TimerKind, TimerRecord};
        use std::time::Duration;
        let a = TimerRecord::arm("a".into(), TimerKind::OneShot, Duration::from_secs(1), 0);
        let b = TimerRecord::arm("b".into(), TimerKind::OneShot, Duration::from_secs(1), 0);
        let (ia, ib) = (a.id.clone(), b.id.clone());
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: a });
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: b });
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerCancelled { ids: vec![ia] });
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.timers[0].id, ib);
    }

    #[test]
    fn coarse_event_filters_streaming_noise_and_input() {
        use models::events::{InputMessageEvent, TextChunkEvent};
        // Streaming noise → None.
        assert!(
            coarse_event(&AgentEvent::TextChunk(TextChunkEvent {
                message_id: "m".into(),
                index: 0,
                text: "noise".into(),
            }))
            .is_none()
        );
        // InputMessage is suppressed from the persistence stream (persisted by the
        // actor instead).
        assert!(
            coarse_event(&AgentEvent::InputMessage(InputMessageEvent {
                message_id: "m".into(),
                input: AgentInput::user_message("m", "hi"),
            }))
            .is_none()
        );
    }
}
