use crate::agent_actor::{AgentActor, AgentCommand, AgentParams};
use crate::context::{AgentRuntimeContext, WorkflowRuntimeContext};
use actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use async_trait::async_trait;
use models::workflow::{WorkflowAgentDef, WorkflowDefinition, WorkflowTransition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Commands accepted by a [`WorkflowActor`].
///
/// The first four are operator-facing; the rest are sent by child [`AgentActor`]s
/// reporting their outcome.
pub enum WorkflowCommand {
    /// Begin the workflow at its start agent.
    Start { input: String },
    /// Cancel the current agent and suspend the workflow.
    Cancel,
    /// Resume a suspended or awaiting-input workflow with a message.
    Resume { message: String },
    /// Fork from a prior session's history, injecting a correction.
    Fork {
        from_session_id: Uuid,
        message: String,
    },

    /// An agent produced its structured output. The workflow evaluates the agent's
    /// transitions against it to pick the next agent, or finishes.
    AgentConcluded { session_id: Uuid, output: Value },
    /// An agent paused to ask the user a question.
    AgentAsked {
        session_id: Uuid,
        tool_call_id: Option<String>,
        question: String,
    },
    /// An agent run failed.
    AgentFailed {
        session_id: Uuid,
        error: String,
        recoverable: bool,
    },
}

/// Events that drive the workflow status machine. Persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkflowDomainEvent {
    WorkflowStarted,
    AgentStarted {
        agent_name: String,
        session_id: Uuid,
        input: String,
    },
    AgentTransitioned {
        from: String,
        to: String,
        from_session: Uuid,
        to_session: Uuid,
        /// The transition condition that matched (`None` = unconditional).
        condition: Option<String>,
    },
    WorkflowFinished {
        output: Value,
    },
    WorkflowSuspended,
    WorkflowFailed {
        error: String,
        recoverable: bool,
    },
    WorkflowPaused {
        session_id: Uuid,
        tool_call_id: Option<String>,
    },
    WorkflowResumed,
}

/// Lifecycle status of a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowStatus {
    Pending,
    Running,
    Suspended,
    AwaitingUserInput,
    Finished,
    Failed,
}

/// Live push signal for an out-of-band observer (e.g. the CLI control loop).
///
/// Emitted on the command path only — never from [`WorkflowActor::apply_event`],
/// which also runs during replay and would re-fire on every recovery. The journal
/// remains the durable source of truth; this channel is best-effort.
#[derive(Debug, Clone)]
pub enum WorkflowNotification {
    /// An agent paused to ask the user a question (the `ask`-kind conclude payload).
    AwaitingUserInput { question: String },
    /// The workflow was suspended (cancel, or a recoverable agent failure).
    Suspended,
    /// The workflow finished with this output.
    Finished { output: Value },
    /// The workflow failed terminally.
    Failed { error: String },
}

/// Persisted workflow state — purely a function of the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    pub status: WorkflowStatus,
    pub current_agent: Option<String>,
    pub current_session_id: Option<Uuid>,
    /// The tool-call id awaiting a user reply while `AwaitingUserInput`. Persisted
    /// (not just in-memory) so `resume` works after recovery in a fresh process.
    #[serde(default)]
    pub pending_tool_call: Option<String>,
}

impl Default for WorkflowState {
    fn default() -> Self {
        Self {
            status: WorkflowStatus::Pending,
            current_agent: None,
            current_session_id: None,
            pending_tool_call: None,
        }
    }
}

/// Orchestrates a [`WorkflowDefinition`]: spawns one [`AgentActor`] per session,
/// routes an agent's structured output to the next agent via its transition rules,
/// and owns the error/interruption model.
pub struct WorkflowActor {
    rt: WorkflowRuntimeContext,
    def: WorkflowDefinition,
    /// The workflow run id (becomes the `id` of this actor's `PersistenceId`).
    run_id: String,
    current_child: Option<ActorRef<AgentCommand>>,
}

impl WorkflowActor {
    pub fn new(
        run_id: impl Into<String>,
        def: WorkflowDefinition,
        rt: WorkflowRuntimeContext,
    ) -> Self {
        Self {
            rt,
            def,
            run_id: run_id.into(),
            current_child: None,
        }
    }

    /// The journal identity of a workflow run: kind `"workflow"`, id = the run/job
    /// id. Lets the supervisor replay a job's workflow event log for `logs` history
    /// without hardcoding the kind string.
    pub fn persistence_id_for(run_id: &str) -> PersistenceId {
        PersistenceId::new("workflow", run_id.to_string())
    }

    fn agent_def(&self, name: &str) -> Option<&WorkflowAgentDef> {
        self.def.agents.iter().find(|a| a.name == name)
    }

    fn spawn_agent(
        &self,
        ctx: &ActorContext<Self>,
        agent_def: &WorkflowAgentDef,
        session_id: Uuid,
    ) -> Result<ActorRef<AgentCommand>, String> {
        let provider = self
            .rt
            .provider_for(&agent_def.model)
            .ok_or_else(|| format!("no provider registered for model '{}'", agent_def.model))?;
        let toolbox = self
            .rt
            .toolbox_factory
            .for_agent(agent_def, self.rt.runtime_client.clone());
        let agent_ctx = AgentRuntimeContext {
            provider,
            toolbox,
            event_sink: self.rt.event_sink.clone(),
            parent_ref: ctx.self_ref(),
            session_id,
        };
        let params = AgentParams::from_def(agent_def);
        Ok(ctx.spawn(AgentActor::new(agent_ctx, params)))
    }

    fn is_current(&self, state: &WorkflowState, session_id: Uuid) -> bool {
        state.current_session_id == Some(session_id)
    }

    /// Best-effort live status push. A full channel drops the notification (logged);
    /// a closed channel (observer gone, e.g. the CLI already exited) is ignored.
    fn notify(&self, n: WorkflowNotification) {
        use tokio::sync::mpsc::error::TrySendError;
        match self.rt.workflow_events.try_send(n) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("workflow_events channel full; dropping notification");
            }
            Err(TrySendError::Closed(_)) => {}
        }
    }

    /// Stringify an agent's structured output for use as the next agent's input.
    fn output_as_input(output: &Value) -> String {
        output
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| output.to_string())
    }

    async fn on_start(
        &mut self,
        input: String,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<WorkflowDomainEvent> {
        let start_name = self.def.start.clone();
        let Some(agent_def) = self.agent_def(&start_name).cloned() else {
            let error = format!("start agent '{start_name}' not found");
            self.notify(WorkflowNotification::Failed {
                error: error.clone(),
            });
            return CommandEffect::persist_and_stop(vec![WorkflowDomainEvent::WorkflowFailed {
                error,
                recoverable: false,
            }]);
        };
        let session_id = Uuid::new_v4();
        match self.spawn_agent(ctx, &agent_def, session_id) {
            Ok(child) => {
                let _ = child
                    .tell(AgentCommand::Run {
                        input: input.clone(),
                    })
                    .await;
                self.current_child = Some(child);
                CommandEffect::persist(vec![
                    WorkflowDomainEvent::WorkflowStarted,
                    WorkflowDomainEvent::AgentStarted {
                        agent_name: start_name,
                        session_id,
                        input,
                    },
                ])
            }
            Err(error) => {
                self.notify(WorkflowNotification::Failed {
                    error: error.clone(),
                });
                CommandEffect::persist_and_stop(vec![WorkflowDomainEvent::WorkflowFailed {
                    error,
                    recoverable: false,
                }])
            }
        }
    }

    async fn on_concluded(
        &mut self,
        state: &WorkflowState,
        session_id: Uuid,
        output: Value,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<WorkflowDomainEvent> {
        if !self.is_current(state, session_id) {
            return CommandEffect::none();
        }
        let Some(from_agent) = state.current_agent.clone() else {
            self.notify(WorkflowNotification::Finished {
                output: output.clone(),
            });
            return CommandEffect::persist_and_stop(vec![WorkflowDomainEvent::WorkflowFinished {
                output,
            }]);
        };
        let transitions = self
            .agent_def(&from_agent)
            .and_then(|d| d.transitions.clone())
            .unwrap_or_default();

        match find_next_transition(&transitions, &output) {
            None => {
                self.notify(WorkflowNotification::Finished {
                    output: output.clone(),
                });
                CommandEffect::persist_and_stop(vec![WorkflowDomainEvent::WorkflowFinished {
                    output,
                }])
            }
            Some((to, condition)) => {
                let Some(to_def) = self.agent_def(&to).cloned() else {
                    let error = format!("transition target agent '{to}' not found");
                    self.notify(WorkflowNotification::Failed {
                        error: error.clone(),
                    });
                    return CommandEffect::persist_and_stop(vec![
                        WorkflowDomainEvent::WorkflowFailed {
                            error,
                            recoverable: false,
                        },
                    ]);
                };
                let to_session = Uuid::new_v4();
                let input = Self::output_as_input(&output);
                match self.spawn_agent(ctx, &to_def, to_session) {
                    Ok(child) => {
                        let _ = child
                            .tell(AgentCommand::Run {
                                input: input.clone(),
                            })
                            .await;
                        self.current_child = Some(child);
                        // Persist (not snapshot): the workflow event log is tiny (a
                        // handful of events per run) and retaining it in full lets
                        // `october job logs` replay the per-job history — every
                        // AgentStarted/AgentTransitioned — after compaction would
                        // otherwise have discarded it.
                        CommandEffect::persist(vec![
                            WorkflowDomainEvent::AgentTransitioned {
                                from: from_agent,
                                to: to.clone(),
                                from_session: session_id,
                                to_session,
                                condition,
                            },
                            WorkflowDomainEvent::AgentStarted {
                                agent_name: to,
                                session_id: to_session,
                                input,
                            },
                        ])
                    }
                    Err(error) => {
                        self.notify(WorkflowNotification::Failed {
                            error: error.clone(),
                        });
                        CommandEffect::persist_and_stop(vec![WorkflowDomainEvent::WorkflowFailed {
                            error,
                            recoverable: false,
                        }])
                    }
                }
            }
        }
    }

    async fn on_resume(
        &mut self,
        state: &WorkflowState,
        message: String,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<WorkflowDomainEvent> {
        match state.status {
            WorkflowStatus::AwaitingUserInput => {
                // Read the awaiting tool-call id from persisted state so resume works
                // after recovery in a fresh process (in-memory fields are gone then).
                let Some(tool_call_id) = state.pending_tool_call.clone() else {
                    return CommandEffect::none();
                };
                let Some(session_id) = state.current_session_id else {
                    return CommandEffect::none();
                };
                let Some(agent_name) = state.current_agent.clone() else {
                    return CommandEffect::none();
                };
                // Re-spawn the agent (recovering its conversation from the session
                // journal) when we no longer hold a live child handle.
                let child = match self.current_child.clone() {
                    Some(child) => child,
                    None => {
                        let Some(agent_def) = self.agent_def(&agent_name).cloned() else {
                            return CommandEffect::none();
                        };
                        match self.spawn_agent(ctx, &agent_def, session_id) {
                            Ok(child) => {
                                self.current_child = Some(child.clone());
                                child
                            }
                            Err(error) => {
                                self.notify(WorkflowNotification::Failed {
                                    error: error.clone(),
                                });
                                return CommandEffect::persist_and_stop(vec![
                                    WorkflowDomainEvent::WorkflowFailed {
                                        error,
                                        recoverable: false,
                                    },
                                ]);
                            }
                        }
                    }
                };
                let _ = child
                    .tell(AgentCommand::InjectToolResult {
                        tool_call_id,
                        content: message,
                    })
                    .await;
                // Do NOT persist a transition here: `tell` only enqueues, so a crash
                // before the agent durably records the injected result would lose the
                // reply and wedge the run. Stay `AwaitingUserInput` (pending_tool_call
                // intact) so resume is idempotent; the agent's own conclude/ask/fail
                // persists the real next state.
                CommandEffect::none()
            }
            WorkflowStatus::Suspended => {
                let Some(session_id) = state.current_session_id else {
                    return CommandEffect::none();
                };
                let Some(agent_name) = state.current_agent.clone() else {
                    return CommandEffect::none();
                };
                let child = match self.current_child.clone() {
                    Some(child) => child,
                    None => {
                        let Some(agent_def) = self.agent_def(&agent_name).cloned() else {
                            return CommandEffect::none();
                        };
                        match self.spawn_agent(ctx, &agent_def, session_id) {
                            Ok(child) => {
                                self.current_child = Some(child.clone());
                                child
                            }
                            Err(error) => {
                                return CommandEffect::persist_and_stop(vec![
                                    WorkflowDomainEvent::WorkflowFailed {
                                        error,
                                        recoverable: false,
                                    },
                                ]);
                            }
                        }
                    }
                };
                let _ = child.tell(AgentCommand::Run { input: message }).await;
                // Same as the await branch: don't persist `Resumed` optimistically.
                // Stay `Suspended` until the agent's own outcome persists the next
                // state, so a crash mid-resume leaves the run resumable.
                CommandEffect::none()
            }
            WorkflowStatus::Pending
            | WorkflowStatus::Running
            | WorkflowStatus::Finished
            | WorkflowStatus::Failed => CommandEffect::none(),
        }
    }

    async fn on_fork(
        &mut self,
        state: &WorkflowState,
        from_session_id: Uuid,
        message: String,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<WorkflowDomainEvent> {
        if state.status != WorkflowStatus::Suspended {
            return CommandEffect::none();
        }
        let Some(agent_name) = state.current_agent.clone() else {
            return CommandEffect::none();
        };
        let Some(agent_def) = self.agent_def(&agent_name).cloned() else {
            return CommandEffect::none();
        };
        let new_session = Uuid::new_v4();
        if let Err(e) = ctx
            .journal()
            .copy_snapshot(
                &AgentActor::persistence_id_for(from_session_id),
                &AgentActor::persistence_id_for(new_session),
            )
            .await
        {
            return CommandEffect::persist_and_stop(vec![WorkflowDomainEvent::WorkflowFailed {
                error: format!("fork failed: {e}"),
                recoverable: false,
            }]);
        }
        match self.spawn_agent(ctx, &agent_def, new_session) {
            Ok(child) => {
                let _ = child
                    .tell(AgentCommand::Run {
                        input: message.clone(),
                    })
                    .await;
                self.current_child = Some(child);
                // Persist (not snapshot) so the full workflow event log survives for
                // `october job logs` history replay (see the transition path).
                CommandEffect::persist(vec![WorkflowDomainEvent::AgentStarted {
                    agent_name,
                    session_id: new_session,
                    input: message,
                }])
            }
            Err(error) => {
                CommandEffect::persist_and_stop(vec![WorkflowDomainEvent::WorkflowFailed {
                    error,
                    recoverable: false,
                }])
            }
        }
    }
}

/// Evaluate transitions in order against `output`, returning the first whose
/// condition matches as `(to, matched_condition)`. An absent condition is an
/// unconditional catch-all. A condition that errors or yields a non-bool is
/// treated as not matching.
fn find_next_transition(
    transitions: &[WorkflowTransition],
    output: &Value,
) -> Option<(String, Option<String>)> {
    for t in transitions {
        match &t.condition {
            None => return Some((t.to.clone(), None)),
            Some(condition) => {
                let matched = eval::Expr::new(condition)
                    .value("output", output.clone())
                    .exec();
                match matched {
                    Ok(v) if v.as_bool() == Some(true) => {
                        return Some((t.to.clone(), Some(condition.clone())));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(condition, error = %e, "transition condition failed to evaluate");
                    }
                }
            }
        }
    }
    None
}

#[async_trait]
impl EventSourcedActor for WorkflowActor {
    type Command = WorkflowCommand;
    type Event = WorkflowDomainEvent;
    type State = WorkflowState;

    fn persistence_id(&self) -> PersistenceId {
        PersistenceId::new("workflow", self.run_id.clone())
    }

    fn initial_state() -> WorkflowState {
        WorkflowState::default()
    }

    fn apply_event(mut state: WorkflowState, event: WorkflowDomainEvent) -> WorkflowState {
        match event {
            WorkflowDomainEvent::WorkflowStarted => state.status = WorkflowStatus::Running,
            WorkflowDomainEvent::AgentStarted {
                agent_name,
                session_id,
                ..
            } => {
                state.current_agent = Some(agent_name);
                state.current_session_id = Some(session_id);
                state.status = WorkflowStatus::Running;
            }
            WorkflowDomainEvent::AgentTransitioned { to, to_session, .. } => {
                state.current_agent = Some(to);
                state.current_session_id = Some(to_session);
                state.status = WorkflowStatus::Running;
            }
            WorkflowDomainEvent::WorkflowFinished { .. } => state.status = WorkflowStatus::Finished,
            WorkflowDomainEvent::WorkflowSuspended => state.status = WorkflowStatus::Suspended,
            WorkflowDomainEvent::WorkflowFailed { .. } => state.status = WorkflowStatus::Failed,
            WorkflowDomainEvent::WorkflowPaused { tool_call_id, .. } => {
                state.status = WorkflowStatus::AwaitingUserInput;
                state.pending_tool_call = tool_call_id;
            }
            WorkflowDomainEvent::WorkflowResumed => {
                state.status = WorkflowStatus::Running;
                state.pending_tool_call = None;
            }
        }
        state
    }

    async fn handle_command(
        &mut self,
        state: &WorkflowState,
        cmd: WorkflowCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<WorkflowDomainEvent> {
        match cmd {
            WorkflowCommand::Start { input } => self.on_start(input, ctx).await,
            WorkflowCommand::Cancel => {
                if let Some(child) = &self.current_child {
                    let _ = child.tell(AgentCommand::Cancel).await;
                }
                self.notify(WorkflowNotification::Suspended);
                CommandEffect::persist(vec![WorkflowDomainEvent::WorkflowSuspended])
            }
            WorkflowCommand::Resume { message } => self.on_resume(state, message, ctx).await,
            WorkflowCommand::Fork {
                from_session_id,
                message,
            } => self.on_fork(state, from_session_id, message, ctx).await,
            WorkflowCommand::AgentConcluded { session_id, output } => {
                self.on_concluded(state, session_id, output, ctx).await
            }
            WorkflowCommand::AgentFailed {
                session_id,
                error,
                recoverable,
            } => {
                if !self.is_current(state, session_id) {
                    return CommandEffect::none();
                }
                if recoverable {
                    self.notify(WorkflowNotification::Suspended);
                    CommandEffect::persist(vec![WorkflowDomainEvent::WorkflowSuspended])
                } else {
                    self.notify(WorkflowNotification::Failed {
                        error: error.clone(),
                    });
                    CommandEffect::persist_and_stop(vec![WorkflowDomainEvent::WorkflowFailed {
                        error,
                        recoverable,
                    }])
                }
            }
            WorkflowCommand::AgentAsked {
                session_id,
                tool_call_id,
                question,
            } => {
                if !self.is_current(state, session_id) {
                    return CommandEffect::none();
                }
                self.notify(WorkflowNotification::AwaitingUserInput { question });
                CommandEffect::persist(vec![WorkflowDomainEvent::WorkflowPaused {
                    session_id,
                    tool_call_id,
                }])
            }
        }
    }

    /// After recovery, re-spawn the current agent when the workflow was `Running`
    /// so the agent's own recovery re-drives the interrupted turn. No command is
    /// sent: the spawned [`AgentActor`] recovers its history and self-continues via
    /// its `on_recovery_complete`. Suspended / AwaitingUserInput stay dormant until
    /// an explicit `Resume`.
    async fn on_recovery_complete(&mut self, state: &WorkflowState, ctx: &mut ActorContext<Self>) {
        if state.status != WorkflowStatus::Running {
            return;
        }
        let (Some(agent_name), Some(session_id)) =
            (state.current_agent.clone(), state.current_session_id)
        else {
            return;
        };
        let Some(agent_def) = self.agent_def(&agent_name).cloned() else {
            return;
        };
        if let Ok(child) = self.spawn_agent(ctx, &agent_def, session_id) {
            self.current_child = Some(child);
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
    use serde_json::json;

    fn sess() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn started_then_agent_started_sets_running() {
        let s = WorkflowActor::initial_state();
        assert_eq!(s.status, WorkflowStatus::Pending);
        let s = WorkflowActor::apply_event(s, WorkflowDomainEvent::WorkflowStarted);
        assert_eq!(s.status, WorkflowStatus::Running);
        let session = sess();
        let s = WorkflowActor::apply_event(
            s,
            WorkflowDomainEvent::AgentStarted {
                agent_name: "writer".into(),
                session_id: session,
                input: "go".into(),
            },
        );
        assert_eq!(s.current_agent.as_deref(), Some("writer"));
        assert_eq!(s.current_session_id, Some(session));
    }

    #[test]
    fn transition_moves_to_target_agent_and_session() {
        let s = WorkflowActor::initial_state();
        let from = sess();
        let to = sess();
        let s = WorkflowActor::apply_event(
            s,
            WorkflowDomainEvent::AgentStarted {
                agent_name: "a".into(),
                session_id: from,
                input: "x".into(),
            },
        );
        let s = WorkflowActor::apply_event(
            s,
            WorkflowDomainEvent::AgentTransitioned {
                from: "a".into(),
                to: "b".into(),
                from_session: from,
                to_session: to,
                condition: Some("output.score > 80".into()),
            },
        );
        assert_eq!(s.current_agent.as_deref(), Some("b"));
        assert_eq!(s.current_session_id, Some(to));
        assert_eq!(s.status, WorkflowStatus::Running);
    }

    #[test]
    fn pause_then_resume_round_trips_status() {
        let session = sess();
        let mut s = WorkflowActor::initial_state();
        s = WorkflowActor::apply_event(
            s,
            WorkflowDomainEvent::AgentStarted {
                agent_name: "a".into(),
                session_id: session,
                input: "x".into(),
            },
        );
        s = WorkflowActor::apply_event(
            s,
            WorkflowDomainEvent::WorkflowPaused {
                session_id: session,
                tool_call_id: Some("tc".into()),
            },
        );
        assert_eq!(s.status, WorkflowStatus::AwaitingUserInput);
        s = WorkflowActor::apply_event(s, WorkflowDomainEvent::WorkflowResumed);
        assert_eq!(s.status, WorkflowStatus::Running);
    }

    #[test]
    fn finished_and_failed_are_terminal_statuses() {
        let done = WorkflowActor::apply_event(
            WorkflowActor::initial_state(),
            WorkflowDomainEvent::WorkflowFinished {
                output: Value::String("ok".into()),
            },
        );
        assert_eq!(done.status, WorkflowStatus::Finished);
        let failed = WorkflowActor::apply_event(
            WorkflowActor::initial_state(),
            WorkflowDomainEvent::WorkflowFailed {
                error: "boom".into(),
                recoverable: false,
            },
        );
        assert_eq!(failed.status, WorkflowStatus::Failed);
    }

    #[test]
    fn unconditional_transition_always_matches() {
        let transitions = vec![WorkflowTransition {
            to: "next".into(),
            condition: None,
        }];
        let next = find_next_transition(&transitions, &json!({}));
        assert_eq!(next, Some(("next".to_string(), None)));
    }

    #[test]
    fn conditional_transition_matches_on_expression() {
        let transitions = vec![
            WorkflowTransition {
                to: "high".into(),
                condition: Some("output.score > 80".into()),
            },
            WorkflowTransition {
                to: "low".into(),
                condition: None,
            },
        ];
        let high = find_next_transition(&transitions, &json!({"score": 95}));
        assert_eq!(high.unwrap().0, "high");
        let low = find_next_transition(&transitions, &json!({"score": 10}));
        assert_eq!(low.unwrap().0, "low");
    }

    #[test]
    fn no_matching_transition_returns_none() {
        let transitions = vec![WorkflowTransition {
            to: "only".into(),
            condition: Some("output.approved == true".into()),
        }];
        let next = find_next_transition(&transitions, &json!({"approved": false}));
        assert_eq!(next, None);
    }

    #[test]
    fn output_as_input_unwraps_json_string() {
        assert_eq!(
            WorkflowActor::output_as_input(&Value::String("hello".into())),
            "hello"
        );
        let obj = json!({"k": 1});
        assert_eq!(WorkflowActor::output_as_input(&obj), obj.to_string());
    }
}
