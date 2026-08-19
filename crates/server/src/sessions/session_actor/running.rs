//! Workflow runs: starting steps, retrying them, and answering the run graph.
//!
//! The decisions live in the workflow runner; this file is the imperative
//! half — spawn the step's agent, hand it its input, journal the entry.

use super::runner::action::StepStart;
use super::runner::event::RunnerEvent;
use super::runner::state::{RunnerState, WorkflowState};
use super::runner::{WorkflowRunner, step_agent_id};
use super::{CommandEffect, RunCommand, RunnerId, SessionActor, SessionEvent, SessionState};
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::addressing::SessionInbox;
use horsie_actor::{ActorContext, ReplyTo};
use horsie_models::now_ms;

impl SessionActor {
    pub(super) async fn handle_run(
        &mut self,
        state: &SessionState,
        cmd: RunCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            RunCommand::State { reply } => {
                let _ = reply.send(root_run(state).map(|(_, w)| w.run.clone()));
                CommandEffect::none()
            }
            RunCommand::Advance => CommandEffect::persist(self.flush_then_drain(state, ctx).await),
            RunCommand::RetryStep { index, reply } => {
                self.on_retry_step(state, index, reply, ctx).await
            }
            RunCommand::ReconcileInterrupted { run } => {
                let current = match state.record(run).map(|r| &r.state) {
                    Some(RunnerState::Workflow(w)) => w.run.current(),
                    _ => None,
                };
                let Some(index) = current else {
                    return CommandEffect::none();
                };
                CommandEffect::persist(vec![SessionEvent::Runner {
                    id: run,
                    at_ms: now_ms(),
                    event: RunnerEvent::StepCancelled { index },
                }])
            }
        }
    }

    /// Begin one execution of one step: spawn its agent and hand it the input.
    pub(super) async fn start_step(
        &mut self,
        run: RunnerId,
        start: StepStart,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        let StepStart {
            index,
            step,
            agent,
            attempt,
            from,
            via,
            input,
        } = start;
        // The execution is not in the run log yet — the event below is what
        // puts it there — so the role is resolved from the step's name in the
        // graph rather than looked up by the agent's id.
        let runner = WorkflowRunner { id: run };
        let role = runner.role_for_step(self.spec(), state, &step, agent);
        let Some(role) = role else {
            return vec![SessionEvent::Runner {
                id: run,
                at_ms: now_ms(),
                event: RunnerEvent::RunFailed {
                    error: format!("step '{step}' is no longer in this workflow"),
                },
            }];
        };
        let Some(actor) = self.spawn_for_role(ctx, state, role).map(|r| r.actor) else {
            return vec![SessionEvent::Runner {
                id: run,
                at_ms: now_ms(),
                event: RunnerEvent::RunFailed {
                    error: format!("step '{step}' could not be started"),
                },
            }];
        };
        // Queued like anything else addressed to an agent. A step agent is
        // freshly spawned and ready, so it drains this immediately — but it
        // goes through the one door, so a step that is asked something and
        // answered later resumes down the same path.
        if actor
            .tell(AgentCommand::Enqueue {
                item: Incoming::User {
                    id: format!("step:{index}:{attempt}"),
                    text: input.clone(),
                },
                ack: None,
            })
            .await
            .is_err()
        {
            return vec![SessionEvent::Runner {
                id: run,
                at_ms: now_ms(),
                event: RunnerEvent::RunFailed {
                    error: format!("step '{step}' could not be started"),
                },
            }];
        }
        vec![SessionEvent::Runner {
            id: run,
            at_ms: now_ms(),
            event: RunnerEvent::StepStarted {
                index,
                step,
                agent,
                attempt,
                from,
                via,
                input,
            },
        }]
    }

    /// Re-run one execution from the root run's log.
    ///
    /// Appends rather than truncating: earlier attempts stay readable, and the
    /// graph renders them stacked on their node. A run still in flight has its
    /// current step cancelled first — the run's workspace is shared, so two
    /// steps must never be writing to it at once.
    ///
    /// The workspace itself is *not* rolled back. A retried step re-runs
    /// against whatever the previous attempt left on disk; that is the honest
    /// behaviour and the guide says so.
    async fn on_retry_step(
        &mut self,
        state: &SessionState,
        index: u32,
        reply: ReplyTo<Result<(), String>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let Some((run_id, w)) = root_run(state) else {
            let _ = reply.send(Err("this session is not a workflow run".into()));
            return CommandEffect::none();
        };
        let Some(target) = w.run.get(index).cloned() else {
            let _ = reply.send(Err(format!("no step execution at index {index}")));
            return CommandEffect::none();
        };
        let mut events = Vec::new();
        // Cancel whatever is in flight first, so the retry is the only writer.
        if let Some(current) = w.run.current() {
            if let Some(step) = w.run.get(current) {
                self.cancel_agent(super::AgentId(step.agent)).await;
            }
            events.push(SessionEvent::Runner {
                id: run_id,
                at_ms: now_ms(),
                event: RunnerEvent::StepCancelled { index: current },
            });
        }
        let mut next = state.clone();
        for e in &events {
            next.apply(e);
        }
        let (new_index, attempt) = match root_run(&next) {
            Some((_, w)) => (
                w.run.steps.len() as u32,
                w.run.attempts_of(&target.step) + 1,
            ),
            None => (0, 1),
        };
        let _ = reply.send(Ok(()));
        events.extend(
            self.start_step(
                run_id,
                StepStart {
                    index: new_index,
                    step: target.step.clone(),
                    agent: step_agent_id(run_id, new_index),
                    attempt,
                    // The retry sits where the original sat, so the graph
                    // draws it on the same edge rather than inventing a new
                    // one.
                    from: target.from,
                    via: target.via.clone(),
                    input: target.input.clone(),
                },
                &next,
                ctx,
            )
            .await,
        );
        CommandEffect::persist(events)
    }
}

/// The session's root workflow run, when it is one.
fn root_run(state: &SessionState) -> Option<(RunnerId, &WorkflowState)> {
    let (id, record) = state.root()?;
    match &record.state {
        RunnerState::Workflow(w) => Some((id, w)),
        RunnerState::Main(_) | RunnerState::Sub(_) | RunnerState::Fork(_) => None,
    }
}
