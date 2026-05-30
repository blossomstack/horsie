use crate::agent_actor::{AgentActor, AgentCommand, AgentParams};
use crate::context::{AgentRuntimeContext, WorkflowRuntimeContext};
use actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor};
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

/// Persisted workflow state — purely a function of the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    pub status: WorkflowStatus,
    pub current_agent: Option<String>,
    pub current_session_id: Option<Uuid>,
}

impl Default for WorkflowState {
    fn default() -> Self {
        Self {
            status: WorkflowStatus::Pending,
            current_agent: None,
            current_session_id: None,
        }
    }
}

/// Orchestrates a [`WorkflowDefinition`]: spawns one [`AgentActor`] per session,
/// routes an agent's structured output to the next agent via its transition rules,
/// and owns the error/interruption model.
pub struct WorkflowActor {
    rt: WorkflowRuntimeContext,
    def: WorkflowDefinition,
    persistence_id: String,
    current_child: Option<ActorRef<AgentCommand>>,
    pending_tool_call: Option<String>,
}

impl WorkflowActor {
    pub fn new(
        persistence_id: impl Into<String>,
        def: WorkflowDefinition,
        rt: WorkflowRuntimeContext,
    ) -> Self {
        Self {
            rt,
            def,
            persistence_id: persistence_id.into(),
            current_child: None,
            pending_tool_call: None,
        }
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
            return CommandEffect::PersistAndStop(vec![WorkflowDomainEvent::WorkflowFailed {
                error: format!("start agent '{start_name}' not found"),
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
                CommandEffect::Persist(vec![
                    WorkflowDomainEvent::WorkflowStarted,
                    WorkflowDomainEvent::AgentStarted {
                        agent_name: start_name,
                        session_id,
                        input,
                    },
                ])
            }
            Err(error) => {
                CommandEffect::PersistAndStop(vec![WorkflowDomainEvent::WorkflowFailed {
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
            return CommandEffect::None;
        }
        let Some(from_agent) = state.current_agent.clone() else {
            return CommandEffect::PersistAndStop(vec![WorkflowDomainEvent::WorkflowFinished {
                output,
            }]);
        };
        let transitions = self
            .agent_def(&from_agent)
            .and_then(|d| d.transitions.clone())
            .unwrap_or_default();

        match find_next_transition(&transitions, &output) {
            None => CommandEffect::PersistAndStop(vec![WorkflowDomainEvent::WorkflowFinished {
                output,
            }]),
            Some((to, condition)) => {
                let Some(to_def) = self.agent_def(&to).cloned() else {
                    return CommandEffect::PersistAndStop(vec![
                        WorkflowDomainEvent::WorkflowFailed {
                            error: format!("transition target agent '{to}' not found"),
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
                        CommandEffect::PersistAndSnapshot(vec![
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
                        CommandEffect::PersistAndStop(vec![WorkflowDomainEvent::WorkflowFailed {
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
                let Some(tool_call_id) = self.pending_tool_call.take() else {
                    return CommandEffect::None;
                };
                if let Some(child) = &self.current_child {
                    let _ = child
                        .tell(AgentCommand::InjectToolResult {
                            tool_call_id,
                            content: message,
                        })
                        .await;
                }
                CommandEffect::Persist(vec![WorkflowDomainEvent::WorkflowResumed])
            }
            WorkflowStatus::Suspended => {
                let Some(session_id) = state.current_session_id else {
                    return CommandEffect::None;
                };
                let Some(agent_name) = state.current_agent.clone() else {
                    return CommandEffect::None;
                };
                let child = match self.current_child.clone() {
                    Some(child) => child,
                    None => {
                        let Some(agent_def) = self.agent_def(&agent_name).cloned() else {
                            return CommandEffect::None;
                        };
                        match self.spawn_agent(ctx, &agent_def, session_id) {
                            Ok(child) => {
                                self.current_child = Some(child.clone());
                                child
                            }
                            Err(error) => {
                                return CommandEffect::PersistAndStop(vec![
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
                CommandEffect::Persist(vec![WorkflowDomainEvent::WorkflowResumed])
            }
            WorkflowStatus::Pending
            | WorkflowStatus::Running
            | WorkflowStatus::Finished
            | WorkflowStatus::Failed => CommandEffect::None,
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
            return CommandEffect::None;
        }
        let Some(agent_name) = state.current_agent.clone() else {
            return CommandEffect::None;
        };
        let Some(agent_def) = self.agent_def(&agent_name).cloned() else {
            return CommandEffect::None;
        };
        let new_session = Uuid::new_v4();
        if let Err(e) = ctx
            .journal()
            .copy_snapshot(&from_session_id.to_string(), &new_session.to_string())
            .await
        {
            return CommandEffect::PersistAndStop(vec![WorkflowDomainEvent::WorkflowFailed {
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
                CommandEffect::PersistAndSnapshot(vec![WorkflowDomainEvent::AgentStarted {
                    agent_name,
                    session_id: new_session,
                    input: message,
                }])
            }
            Err(error) => {
                CommandEffect::PersistAndStop(vec![WorkflowDomainEvent::WorkflowFailed {
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

    fn persistence_id(&self) -> String {
        self.persistence_id.clone()
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
            WorkflowDomainEvent::WorkflowPaused { .. } => {
                state.status = WorkflowStatus::AwaitingUserInput
            }
            WorkflowDomainEvent::WorkflowResumed => state.status = WorkflowStatus::Running,
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
                CommandEffect::Persist(vec![WorkflowDomainEvent::WorkflowSuspended])
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
                    return CommandEffect::None;
                }
                if recoverable {
                    CommandEffect::Persist(vec![WorkflowDomainEvent::WorkflowSuspended])
                } else {
                    CommandEffect::PersistAndStop(vec![WorkflowDomainEvent::WorkflowFailed {
                        error,
                        recoverable,
                    }])
                }
            }
            WorkflowCommand::AgentAsked {
                session_id,
                tool_call_id,
                ..
            } => {
                if !self.is_current(state, session_id) {
                    return CommandEffect::None;
                }
                self.pending_tool_call = tool_call_id.clone();
                CommandEffect::Persist(vec![WorkflowDomainEvent::WorkflowPaused {
                    session_id,
                    tool_call_id,
                }])
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
