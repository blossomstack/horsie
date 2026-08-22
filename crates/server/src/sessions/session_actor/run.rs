//! The workflow runs this session hosts: its own, and every run its agents
//! invoke mid-session.
//!
//! Reads each run's log, evaluates the transition out of its last concluded
//! step, and decides the next step, the run's end, or its failure. Appends
//! rather than replaces: a loop back onto a step and a retry of one are both
//! new entries, which is what keeps the log replayable and the graph
//! projection lossless.
//!
//! Silent when the forest holds no runs. That check, rather than a branch
//! chosen at construction, is the whole of what makes this component inert in
//! a conversation that has invoked nothing.

use super::component::Component;
use super::context::SessionAgentKind;
use super::{
    AgentAction, AgentKey, AgentPlan, CommandEffect, RunCommand, SessionActor, SessionCommand,
    SessionDomainEvent, SessionState, TurnEnd,
};
use crate::agent_loop::QueueCommand as AgentQueueCommand;
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::addressing::SessionInbox;
use crate::sessions::orchestrator::StepStart;
use crate::sessions::run_forest::{MAX_DEPTH, MAX_LIVE_RUNS, RunId, RunState, STOPPED_ERROR};
use crate::sessions::workflow::{WorkflowOrchestrator, WorkflowRunStatus};
use horsie_actor::ActorContext;
use horsie_actor::ActorRef;
use horsie_actor::EventSourcedActor;
use horsie_actor::ReplyTo;
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

/// WorkflowRuns.
pub(super) struct WorkflowRuns;

impl WorkflowRuns {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: RunCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            RunCommand::Create {
                parent,
                graph,
                reply,
            } => {
                // One depth rule for every delegation edge, which is what
                // bounds a workflow that invokes itself: each invocation is a
                // level, and the fifth is refused wherever it comes from.
                let Some(parent_depth) = state.forest.depth_of_agent(parent) else {
                    let _ = reply.send(Err("caller is not a known agent".to_string()));
                    return CommandEffect::none();
                };
                if parent_depth >= MAX_DEPTH {
                    let _ = reply.send(Err(format!("max delegation depth {MAX_DEPTH} reached")));
                    return CommandEffect::none();
                }
                if state.forest.live_run_count() >= MAX_LIVE_RUNS {
                    let _ = reply.send(Err(format!(
                        "{MAX_LIVE_RUNS} workflow runs already live in this session"
                    )));
                    return CommandEffect::none();
                }
                // Persist first, start second: a crash between the two replays
                // as a pending run the next boundary simply starts — never an
                // untracked one.
                let id = Uuid::new_v4();
                let created = SessionDomainEvent::RunCreated {
                    at_ms: now_ms(),
                    id,
                    parent,
                    graph,
                };
                let (tx, rx) = oneshot::channel();
                let self_ref = actor.me(ctx);
                tokio::spawn(async move {
                    let persisted = rx.await.unwrap_or_else(|_| {
                        Err(horsie_actor::JournalError::Backend(
                            "run ack channel closed".to_string(),
                        ))
                    });
                    let _ = self_ref
                        .tell(SessionCommand::Run(RunCommand::FinishCreate {
                            id,
                            reply,
                            persisted,
                        }))
                        .await;
                });
                CommandEffect::persist(vec![created]).and_ack(ReplyTo::from_sender(tx))
            }
            RunCommand::FinishCreate {
                id,
                reply,
                persisted,
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist workflow run: {e}")));
                    return CommandEffect::none();
                }
                // The id travels now; the boundary drain below is what starts
                // the pending run's first step.
                let _ = reply.send(Ok(id));
                CommandEffect::persist(actor.flush_then_drain(state, ctx).await)
            }
            RunCommand::Status { caller, run, reply } => {
                // Visibility is the caller's descendant closure, like a
                // subagent's: the invoker and its ancestors see the run,
                // siblings do not — and an out-of-subtree id is refused
                // without confirming the run exists.
                let visible = state
                    .forest
                    .entry(RunId(run))
                    .and_then(|e| e.parent)
                    .is_some_and(|p| p == caller || state.forest.descends_from(p, caller));
                let rendered = match visible {
                    true => state
                        .forest
                        .render_run(RunId(run))
                        .ok_or_else(|| format!("no such workflow run: {run}")),
                    false => Err(format!("no such workflow run: {run}")),
                };
                let _ = reply.send(rendered);
                CommandEffect::none()
            }
            // The HTTP surface addresses the session's own run; invoked runs
            // report to their invoker instead of to a route.
            RunCommand::State { reply } => {
                let _ = reply.send(state.forest.root_workflow().map(|(_, w)| w.run.clone()));
                CommandEffect::none()
            }
            RunCommand::Advance => CommandEffect::persist(actor.flush_then_drain(state, ctx).await),
            RunCommand::RetryStep { index, reply } => {
                actor.on_retry_step(state, index, reply, ctx).await
            }
            RunCommand::ReconcileInterrupted => {
                let cancelled: Vec<SessionDomainEvent> = state
                    .forest
                    .in_flight_steps()
                    .into_iter()
                    .map(|(run, index, _)| SessionDomainEvent::StepCancelled {
                        at_ms: now_ms(),
                        run: run.0,
                        index,
                    })
                    .collect();
                if cancelled.is_empty() {
                    return CommandEffect::none();
                }
                CommandEffect::persist(cancelled)
            }
        }
    }
}

/// Handlers that belong to this component but act on the actor's own
/// fields — the roster, the supervisor link, the spawn helpers. An inherent
/// `impl` in a child module sees them, so moving the code needed no plumbing.
impl SessionActor {
    /// Begin one execution of one step: spawn its agent and hand it the input.
    pub(super) async fn start_step(
        &mut self,
        start: StepStart,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionDomainEvent> {
        let StepStart {
            run,
            index,
            step,
            agent,
            attempt,
            from,
            via,
            input,
        } = start;
        // The name is not in the log yet — this event is what puts it there —
        // so it is handed to the spawner rather than looked up.
        let Some(actor) = self.spawn_step_agent(ctx, state, run, agent, &step) else {
            return vec![SessionDomainEvent::RunFailed {
                at_ms: now_ms(),
                run: run.0,
                error: format!("step '{step}' is no longer in this workflow"),
            }];
        };
        // Queued like anything else addressed to an agent. A step agent is
        // freshly spawned and ready, so it drains this immediately — but it
        // goes through the one door, so a step that is asked something and
        // answered later resumes down the same path.
        if actor
            .tell(AgentCommand::Queue(AgentQueueCommand::Enqueue {
                item: Incoming::User {
                    id: format!("step:{index}:{attempt}"),
                    text: input.clone(),
                },
                ack: None,
            }))
            .await
            .is_err()
        {
            return vec![SessionDomainEvent::RunFailed {
                at_ms: now_ms(),
                run: run.0,
                error: format!("step '{step}' could not be started"),
            }];
        }
        vec![SessionDomainEvent::StepStarted {
            at_ms: now_ms(),
            run: run.0,
            index,
            step,
            agent,
            attempt,
            from,
            via,
            input,
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
    pub(super) async fn on_retry_step(
        &mut self,
        state: &SessionState,
        index: u32,
        reply: ReplyTo<Result<(), String>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let Some((run_id, w)) = state.forest.root_workflow() else {
            let _ = reply.send(Err("this session is not a workflow run".into()));
            return CommandEffect::none();
        };
        let Some(target) = w.run.get(index).cloned() else {
            let _ = reply.send(Err(format!("no step execution at index {index}")));
            return CommandEffect::none();
        };
        let mut events = Vec::new();
        // Cancel whatever is in flight first, so the retry is the only writer.
        // The superseded step's own outstanding children go with it: their
        // results would otherwise arrive addressed to an execution the retry
        // has replaced.
        if let Some(current) = w.run.current() {
            if let Some(step) = w.run.get(current) {
                self.cancel_agent(AgentKey::Step(step.agent)).await;
                events.extend(self.cancel_descendants(state, step.agent).await);
            }
            events.push(SessionDomainEvent::StepCancelled {
                at_ms: now_ms(),
                run: run_id.0,
                index: current,
            });
        }
        let mut next = state.clone();
        for e in &events {
            next = SessionActor::apply_event(next, e.clone());
        }
        let (new_index, attempt) = next
            .forest
            .workflow(run_id)
            .map(|w| {
                (
                    w.run.steps.len() as u32,
                    w.run.attempts_of(&target.step) + 1,
                )
            })
            .unwrap_or((0, 1));
        let _ = reply.send(Ok(()));
        events.extend(
            self.start_step(
                StepStart {
                    run: run_id,
                    index: new_index,
                    step: target.step.clone(),
                    agent: crate::sessions::workflow::WorkflowRunSpec::step_agent_id(
                        run_id.0, new_index,
                    ),
                    attempt,
                    // The retry sits where the original sat, so the graph draws
                    // it on the same edge rather than inventing a new one.
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

    /// One step's outcome. Mechanical: map it onto the log entry that records
    /// it, then let the orchestrator read the folded state and decide what runs
    /// next. Every branching decision — which transition, whether the run is
    /// over — is in the driver, not here.
    pub(super) async fn on_step_outcome(
        &mut self,
        state: &SessionState,
        run: RunId,
        index: u32,
        agent: Uuid,
        end: TurnEnd,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let (events, advance) = match end {
            TurnEnd::Concluded { output } => (
                vec![SessionDomainEvent::StepConcluded {
                    at_ms: now_ms(),
                    run: run.0,
                    index,
                    output,
                }],
                true,
            ),
            TurnEnd::Asked => {
                (
                    vec![SessionDomainEvent::AskRecorded {
                        at_ms: now_ms(),
                        agent,
                    }],
                    // The step is still running, parked on its question. The
                    // answer — sent to the step agent, which owns it — resumes
                    // it; nothing else starts meanwhile.
                    false,
                )
            }
            // A step that fails fails the run, terminal or not. Retrying it is a
            // decision for a person: the shared workspace holds whatever the
            // failed attempt left behind, so re-running blind would redo
            // half-finished work.
            TurnEnd::Failed { error, .. } => (
                vec![SessionDomainEvent::StepFailed {
                    at_ms: now_ms(),
                    run: run.0,
                    index,
                    error,
                }],
                // An invoked run's failure is a report its invoker is owed,
                // and delivery is a boundary action.
                true,
            ),
            // A step that armed a timer, or is waiting on subagents it spawned,
            // ends its turn without finishing — and that is not the step
            // ending. It stays running until whatever it is waiting for wakes
            // it, exactly as a parked question does. This used to fail the run
            // outright, which made a step that suspended itself deliberately
            // indistinguishable from one that crashed.
            TurnEnd::Parked => (Vec::new(), false),
            // A step the process died inside is suspended by
            // `WorkflowRuns::on_load`, which is the state a retry can move.
            // Recording it a second time from the step agent's own recovery
            // would append a second log entry for one execution — and a step
            // agent stays cold, so its report arrives long after the repair.
            TurnEnd::Interrupted => return CommandEffect::none(),
        };
        match advance {
            true => self.persist_and_advance(state, events, ctx).await,
            false => CommandEffect::persist(events),
        }
    }

    /// Cancel everything running under `of` — its subagents, the workflows it
    /// invoked, their in-flight steps, and everything under those — and return
    /// the events that record it.
    ///
    /// The whole closure, not one level: a person stopping an agent means the
    /// delegation is over, and a grandchild left running would go on writing
    /// to the shared workspace for a requester that walked away. Each stopped
    /// child still reports [`STOPPED_ERROR`] to its own parent through the
    /// ordinary owed-delivery path — delivered as data, never as a failure of
    /// the parent itself.
    pub(super) async fn cancel_descendants(
        &mut self,
        state: &SessionState,
        of: Uuid,
    ) -> Vec<SessionDomainEvent> {
        let mut events = Vec::new();
        for id in state.forest.descendant_entries(of) {
            let Some(entry) = state.forest.entry(id) else {
                continue;
            };
            match &entry.state {
                RunState::Sub(sub) => {
                    if sub.status == crate::sessions::run_forest::SubAgentStatus::Running {
                        self.cancel_agent(AgentKey::Sub(id.0)).await;
                        events.push(SessionDomainEvent::SubAgentFailed {
                            at_ms: now_ms(),
                            id: id.0,
                            error: STOPPED_ERROR.to_string(),
                        });
                    }
                }
                RunState::Workflow(w) => {
                    if w.run.status.is_terminal() {
                        continue;
                    }
                    if let Some(index) = w.run.current() {
                        if let Some(step) = w.run.get(index) {
                            self.cancel_agent(AgentKey::Step(step.agent)).await;
                        }
                        events.push(SessionDomainEvent::StepCancelled {
                            at_ms: now_ms(),
                            run: id.0,
                            index,
                        });
                    }
                    events.push(SessionDomainEvent::RunFailed {
                        at_ms: now_ms(),
                        run: id.0,
                        error: STOPPED_ERROR.to_string(),
                    });
                }
                // A fork is a conversation of its own, not delegated work: it
                // outlives whoever branched it, exactly as deleting a parent
                // fork leaves its children.
                RunState::Fork(_) | RunState::Main(_) => {}
            }
        }
        events
    }

    /// Spawn the agent for one execution of a workflow step.
    ///
    /// Differs from a subagent in three ways, all of them the point: it runs
    /// with its *own* preset's settings rather than the session's, it carries
    /// what the step promises to return so `submit_result` is typed, and it is
    /// keyed as a step so its spawns hang off it.
    ///
    /// The definition comes from the *run entry's* snapshot, never the session
    /// spec: an invoked run carries its own graph, and even the root run's
    /// entry holds the same `Arc` the spec does.
    pub(super) fn spawn_step_agent(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        run: RunId,
        agent_id: Uuid,
        step_name: &str,
    ) -> Option<ActorRef<AgentCommand>> {
        let graph = state.run_graph(run)?;
        let step = graph.step(step_name)?.clone();
        // A step runs under its own preset, resolved from the run snapshot:
        // at spawn the execution is not in the run log yet (the event that
        // records it persists after the agent exists), so the id cannot be
        // looked up — the step's own spec is the same settings a later read
        // resolves by id.
        let settings = step.settings.clone();
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::Step(agent_id),
                settings,
                step_result: crate::sessions::session_actor::context::StepResultDef {
                    outcomes: step.outcomes.clone(),
                    fields: step.fields.clone(),
                    interactive: step.interactive,
                },
                agent_type: None,
            },
        )
        .map(|resident| resident.actor)
    }
}

impl Component for WorkflowRuns {
    /// The next step, the end, or the failure — of every live run. Sibling
    /// runs progress concurrently; within one run, one step at a time.
    fn actions(state: &SessionState) -> Vec<AgentAction> {
        state
            .forest
            .workflows()
            .flat_map(|(id, w)| WorkflowOrchestrator::new(id, w.graph.clone()).step_actions(&w.run))
            .collect()
    }

    /// A run is created and then left to begin by itself, with no first message
    /// to trigger it. This is the one place a session starts work at load.
    ///
    /// A step left in flight is the other thing recovery finds, and it is not
    /// resumed: how far it got is unknowable, and its effect on the shared
    /// workspace with it. It is suspended instead, which is what makes a retry
    /// the person's decision. Without this the entry stayed `Running`, so
    /// `current()` never cleared and the run started nothing ever again.
    fn on_load(state: &SessionState) -> Option<SessionCommand> {
        if state.forest.has_step_in_flight() {
            return Some(SessionCommand::Run(RunCommand::ReconcileInterrupted));
        }
        state
            .forest
            .workflows()
            .any(|(_, w)| w.run.status == WorkflowRunStatus::Pending)
            .then_some(SessionCommand::Run(RunCommand::Advance))
    }

    fn busy(state: &SessionState) -> bool {
        state.forest.has_step_in_flight()
    }

    /// The run logs. Appended, never replaced — a loop back onto a step and a
    /// retry of one are both new entries, which is what keeps this a pure fold.
    ///
    /// Pure, and an associated function rather than a method: replay runs with
    /// no instance in scope, which is what makes a recovered session and a live
    /// one follow the same path.
    // The fallthrough is unreachable by construction: `SessionActor::apply_event`
    // matches every variant explicitly and routes each to exactly one component,
    // so a newly added event fails to compile *there* — which is where it should
    // be classified — rather than silently reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::RunCreated {
                at_ms,
                id,
                parent,
                graph,
            } => {
                state.forest.apply_run_created(
                    RunId(id),
                    parent,
                    graph.workflow.clone(),
                    graph,
                    at_ms,
                );
            }
            SessionDomainEvent::StepStarted {
                at_ms,
                run,
                step,
                agent,
                attempt,
                from,
                via,
                input,
                ..
            } => {
                state.forest.apply_step_started(
                    RunId(run),
                    step,
                    agent,
                    attempt,
                    from,
                    via,
                    input,
                    at_ms,
                );
            }
            SessionDomainEvent::StepConcluded {
                at_ms,
                run,
                index,
                output,
            } => {
                state
                    .forest
                    .apply_step_concluded(RunId(run), index, output, at_ms);
            }
            SessionDomainEvent::StepFailed {
                at_ms,
                run,
                index,
                error,
            } => {
                state
                    .forest
                    .apply_step_failed(RunId(run), index, error, at_ms);
            }
            SessionDomainEvent::StepCancelled { at_ms, run, index } => {
                state.forest.apply_step_cancelled(RunId(run), index, at_ms);
            }
            SessionDomainEvent::RunFinished { run, output, .. } => {
                state.forest.apply_run_finished(RunId(run), output);
            }
            SessionDomainEvent::RunFailed { run, error, .. } => {
                state.forest.apply_run_failed(RunId(run), error);
            }
            SessionDomainEvent::RunNotified { run, .. } => {
                state.forest.apply_run_notified(RunId(run));
            }
            other => unreachable!("WorkflowRuns was handed {other:?}"),
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
    //! The graph: what starts a run, how a transition routes, and what a retry
    //! appends.
    use super::super::testing::*;
    use super::super::*;
    use crate::sessions::session_actor::testing::seed_session;

    use horsie_agentcore::LlmProvider;

    use std::sync::Arc;
    use uuid::Uuid;

    /// A run that reached a terminal step with no error says so, and keeps
    /// saying so once it is cold. `Idle` could not tell it apart from a run
    /// that stopped part-way and is waiting for someone to retry a step.
    #[tokio::test]
    async fn a_completed_run_reports_finished() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"outcome": "p0"})))]).then_repeating_with(
                // Every later step submits too: a step ends by calling
                // `submit_result`, and a turn of plain text with nothing to
                // wake it is now a mistake the actor nudges.
                || Ok(concludes(serde_json::json!({"description": "fixed"}))),
            ),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let state = wait_for_state(&journal, id, "the run to finish", |s| {
            s.forest.root_workflow().is_some_and(|(_, w)| {
                w.run.status == crate::sessions::workflow::WorkflowRunStatus::Finished
            })
        })
        .await;
        assert_eq!(
            state.status(),
            crate::sessions::spec::SessionStatus::Finished,
            "a run that completed is not merely idle"
        );
    }

    /// The whole point: a run starts itself, its first step's output picks the
    /// branch, and the branch's step ends the run.
    #[tokio::test]
    async fn a_run_starts_itself_and_routes_on_its_first_steps_output() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"outcome": "p0"})))]).then_repeating_with(
                // Every later step submits too: a step ends by calling
                // `submit_result`, and a turn of plain text with nothing to
                // wake it is now a mistake the actor nudges.
                || Ok(concludes(serde_json::json!({"description": "fixed"}))),
            ),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;

        // Nobody sent a message: creating the run is what starts it.
        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;

        let visited: Vec<&str> = run.steps.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(
            visited,
            vec!["triage", "fix"],
            "p0 must route to `fix`; triage concluded with {:?}",
            run.steps[0].output
        );
        // The condition that matched is recorded, which is what draws the edge.
        assert_eq!(run.steps[1].via.as_deref(), Some("outcome in [p0]"));
        assert_eq!(run.steps[1].from, Some(0));
        // Each step is its own agent, derived from the session and the index.
        assert_eq!(
            run.steps[0].agent,
            crate::sessions::workflow::WorkflowRunSpec::step_agent_id(id, 0)
        );
        assert_ne!(run.steps[0].agent, run.steps[1].agent);
        // The second step was handed the first's output under a header.
        assert!(
            run.steps[1].input.contains("## Input from step `triage`"),
            "{}",
            run.steps[1].input
        );
        assert!(run.steps[1].input.starts_with("Fix it."));
    }

    /// The `else` branch, and the run's output being the last step's.
    #[tokio::test]
    async fn a_non_matching_condition_takes_the_catch_all() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"outcome": "p2"})))])
                .then_repeating_with(|| Ok(concludes(serde_json::json!({"description": "filed"})))),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;
        let visited: Vec<&str> = run.steps.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(visited, vec!["triage", "file"]);
        assert!(run.steps[1].via.is_none());
    }

    /// A step ends when it calls `submit_result` — a turn ending is not a step
    /// ending. When nothing would wake the agent, the model is nudged rather
    /// than the step failed outright: one forgetful turn should not kill a run
    /// fifteen steps deep with real changes on the shared workspace.
    #[tokio::test]
    async fn a_step_that_ends_a_turn_with_text_is_nudged_and_then_submits() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let text = || {
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "I think that's everything.".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        };
        let provider = MockProvider::scripted(
            Script::of([
                // The step believes it is done but says so in prose.
                text(),
                // Nudged, it submits.
                Ok(concludes(serde_json::json!({"outcome": "p2"}))),
            ])
            .then_repeating_with(|| Ok(concludes(serde_json::json!({"description": "filed"})))),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;
        assert_eq!(
            run.steps[0].status,
            crate::sessions::workflow::StepStatus::Concluded,
            "the nudged step still concluded: {:?}",
            run.steps[0]
        );
        assert_eq!(
            run.steps[0].output.as_ref().and_then(|o| o.get("outcome")),
            Some(&serde_json::json!("p2")),
            "and the result it submitted after the nudge is the one that routed"
        );
    }

    /// A step that armed a timer and then stopped talking is *parked*, not
    /// stuck: the timer will wake it. Nudging here would push a model that
    /// deliberately suspended itself into submitting a result it does not have
    /// yet, and failing the step would end a run that was working correctly.
    #[tokio::test]
    async fn a_step_that_ends_a_turn_holding_a_timer_is_parked_not_nudged() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::ToolCall(
                        horsie_agentcore::ToolCallPart {
                            id: "t-1".into(),
                            name: "set_timer".into(),
                            input: serde_json::json!({
                                "kind": "one_shot",
                                "after_secs": 3600,
                                "label": "check back",
                                "message": "see whether CI went green",
                            }),
                        },
                    )],
                    stop_reason: horsie_agentcore::StopReason::ToolUse,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                }),
                // Then it stops talking, holding the timer.
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "I'll pick this up when the timer fires.".to_string(),
                        },
                    )],
                    stop_reason: horsie_agentcore::StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                }),
            ])
            // Anything past this is the bug: a parked step must not run again
            // until its timer fires.
            .then_repeating_with(|| Ok(concludes(serde_json::json!({"outcome": "p0"})))),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let started = wait_for_run(&journal, id, |r| !r.steps.is_empty()).await;
        let step = wait_for_agent(&journal, started.steps[0].agent, |s| s.parked).await;
        assert_eq!(step.nudges, 0, "a park is not a mistake to be corrected");
        assert_eq!(step.timers.len(), 1, "and the timer is still armed");
        let run = crate::sessions::events::fold_session_state(&journal, id)
            .await
            .forest
            .root_workflow()
            .expect("the run exists")
            .1
            .run
            .clone();
        assert_eq!(
            run.steps[0].status,
            crate::sessions::workflow::StepStatus::Running,
            "the step is still running, waiting on its timer"
        );
    }

    /// Submitting says the work is done, which makes an armed timer moot. Left
    /// armed it would fire an hour later into a step the run has long moved
    /// past, waking an agent with nothing left to do.
    #[tokio::test]
    async fn submitting_cancels_the_timers_the_step_had_armed() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let arm = || {
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::ToolCall(
                    horsie_agentcore::ToolCallPart {
                        id: "t-1".into(),
                        name: "set_timer".into(),
                        input: serde_json::json!({
                            "kind": "one_shot",
                            "after_secs": 3600,
                            "label": "check back",
                            "message": "see whether CI went green",
                        }),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::ToolUse,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        };
        let provider = MockProvider::scripted(
            Script::of([arm(), Ok(concludes(serde_json::json!({"outcome": "p0"})))])
                .then_repeating_with(|| Ok(concludes(serde_json::json!({"description": "fixed"})))),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;
        let step = crate::sessions::events::fold_agent_state(&journal, run.steps[0].agent).await;
        // The timer really was armed — otherwise this test passes by testing
        // nothing, which is exactly what it did the first time it was written.
        assert!(
            step.log.iter().any(|e| matches!(
                &e.body,
                horsie_agentcore::AgentLogBody::Llm(m)
                    if m.parts.iter().any(|p| matches!(
                        p,
                        horsie_agentcore::ContentPart::ToolCall(c) if c.name == "set_timer"
                    ))
            )),
            "the step never armed a timer, so cancelling one proves nothing"
        );
        assert!(
            step.timers.is_empty(),
            "the concluded step still holds {} armed timer(s)",
            step.timers.len()
        );
    }

    /// A model that never submits fails its step rather than looping for ever.
    /// The second nudge forces `submit_result` in `tool_choice`, so reaching
    /// this means the provider ignored a constraint it is required to honour.
    #[tokio::test]
    async fn a_step_that_never_submits_fails_the_run() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(Script::of([]).then_repeating_with(|| {
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "done I think".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }));
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Failed
        })
        .await;
        let error = run.error.unwrap_or_default();
        assert!(
            error.contains("submit_result"),
            "the failure has to name what was missing: {error}"
        );
    }

    /// Retrying appends an attempt rather than replacing one, so the earlier
    /// attempt stays readable and the graph can stack them.
    #[tokio::test]
    async fn retrying_a_step_appends_an_attempt_on_the_same_edge() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"outcome": "p0"})))]).then_repeating_with(
                // Every later step submits too: a step ends by calling
                // `submit_result`, and a turn of plain text with nothing to
                // wake it is now a mistake the actor nudges.
                || Ok(concludes(serde_json::json!({"description": "fixed"}))),
            ),
        );
        let (_f, session, id, journal) = spawn_run_with_provider(provider).await;
        wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;

        session
            .ask(|reply| SessionCommand::Run(RunCommand::RetryStep { index: 1, reply }))
            .await
            .unwrap()
            .unwrap();
        let run = wait_for_run(&journal, id, |r| r.steps.len() == 3).await;
        assert_eq!(run.steps[2].step, "fix");
        assert_eq!(run.steps[2].attempt, 2, "the retry numbers itself");
        // It sits where the original sat, so it draws on the same edge.
        assert_eq!(run.steps[2].from, run.steps[1].from);
        assert_eq!(run.steps[2].via, run.steps[1].via);
        // The first attempt is untouched.
        assert_eq!(
            run.steps[1].status,
            crate::sessions::workflow::StepStatus::Concluded
        );
    }

    /// A run has no first message to hold it back — `AdvanceRun` fires at load
    /// and starts step one by itself. So it needs the same wait a conversation
    /// gets, and for the same reason: the step would ask for a runtime nobody
    /// had been told to build.
    #[tokio::test]
    async fn a_runs_first_step_waits_for_the_create_too() {
        let f = actor_fixture_blocking_creates().await;
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            crate::sessions::spec::ModelEntry::provider_only(
                Arc::new(EchoProvider) as Arc<dyn LlmProvider>
            ),
        );
        let id = Uuid::new_v4();
        let mut spec = actor_spec_fixture();
        spec.kind = crate::sessions::spec::SessionKind::Workflow {
            run: Arc::new(run_spec_fixture("the build is red")),
        };
        let journal = f.journal();
        let session = f.start(id, spec).await;
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();

        wait_for_state(&journal, id, "a run holding at its create", |s| {
            s.status() == crate::sessions::spec::SessionStatus::Provisioning
        })
        .await;
        let held = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(
            held.forest
                .root_workflow()
                .is_none_or(|(_, w)| w.run.steps.is_empty()),
            "no step may start before the runtime it would run on"
        );

        f.agent.release_creates();
        wait_for_run(&journal, id, |r| !r.steps.is_empty()).await;
    }

    /// A step asks, is answered *without* the caller naming it, and the run
    /// carries on.
    ///
    /// Three separate defects met here. A run has no main agent, so an
    /// unaddressed answer resolved nothing and silently did nothing; the web
    /// client sent exactly that. And nothing ever cleared `AwaitingInput`, so
    /// even a delivered answer resumed the step and then stalled the run at the
    /// step it had just finished.
    #[tokio::test]
    async fn a_parked_step_is_answered_unaddressed_and_the_run_carries_on() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([
                Ok(asks("p0 or p2?")),
                Ok(concludes(serde_json::json!({"outcome": "p0"}))),
            ])
            .then_repeating_with(|| Ok(concludes(serde_json::json!({"description": "fixed"})))),
        );
        let (_f, session, id, journal) = spawn_run_with_provider(provider).await;

        let parked = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::AwaitingInput
        })
        .await;
        assert_eq!(
            parked.steps[0].status,
            crate::sessions::workflow::StepStatus::Running,
            "the step is still running, parked on its question"
        );

        // Unaddressed, which is the case that used to resolve nothing: a run has
        // no main agent, so the step in flight is the only thing this can mean.
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::Answer {
                    agent_id: None,
                    answers: vec![answer(ASK_CALL_ID, "p0")],
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("an unaddressed answer must reach the step in flight");

        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;
        let visited: Vec<&str> = run.steps.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(
            visited,
            vec!["triage", "fix"],
            "the answer decided the branch"
        );
    }

    /// Interrupting a run suspends it. Cancelling the agent was never enough:
    /// the step's entry stayed `Running`, so `current()` never cleared and the
    /// driver started nothing ever again — the run wedged while its page read
    /// "Running".
    #[tokio::test]
    async fn interrupting_a_run_cancels_its_step_and_suspends_it() {
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) =
            spawn_run_with_provider(provider.clone() as Arc<dyn LlmProvider>).await;

        let step = wait_for_run(&journal, id, |r| r.current() == Some(0)).await;
        let agent = step.current_agent().expect("a step in flight has an agent");
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::Stop {
                    agent_id: agent.to_string(),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a step in flight is stoppable");

        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Suspended
        })
        .await;
        assert_eq!(
            run.steps[0].status,
            crate::sessions::workflow::StepStatus::Cancelled
        );
        assert!(
            run.current().is_none(),
            "nothing is in flight, so a retry can move the run"
        );
        provider.release();
    }

    /// A step the process died inside is suspended at load, not resumed: how far
    /// it got is unknowable and its effect on the shared workspace with it. The
    /// guide has always promised this; nothing implemented it, so the entry
    /// stayed `Running` and the run was unrecoverable except through a retry
    /// nobody was told to make.
    #[tokio::test]
    async fn recovery_suspends_a_run_whose_step_was_interrupted() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let mut spec = actor_spec_fixture();
        spec.kind = crate::sessions::spec::SessionKind::Workflow {
            run: Arc::new(run_spec_fixture("the build is red")),
        };
        f.deps
            .runtimes
            .create(&id.to_string(), "i1", "mock", &spec)
            .await
            .expect("create");

        // A journal that stops mid-step, which is exactly what a crash leaves.
        let journal = f.journal();
        let _session = seed_session(
            &f,
            id,
            spec,
            &[SessionDomainEvent::StepStarted {
                at_ms: 0,
                run: id,
                index: 0,
                step: "triage".into(),
                agent: crate::sessions::workflow::WorkflowRunSpec::step_agent_id(id, 0),
                attempt: 1,
                from: None,
                via: None,
                input: "Triage it.".into(),
            }],
        )
        .await;

        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Suspended
        })
        .await;
        assert_eq!(
            run.steps[0].status,
            crate::sessions::workflow::StepStatus::Cancelled
        );
        assert_eq!(run.steps.len(), 1, "recovery starts nothing by itself");
    }

    /// A finished run's step transcript survives the session unloading.
    ///
    /// Every agent-scoped read resolves through `resolve_agent`, and a reloaded
    /// run holds an empty roster — so before a step could be spawned on demand,
    /// the step page went permanently blank the moment the session went idle.
    #[tokio::test]
    async fn a_cold_steps_transcript_is_still_readable_after_a_reload() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"outcome": "p0"})))]).then_repeating_with(
                // Every later step submits too: a step ends by calling
                // `submit_result`, and a turn of plain text with nothing to
                // wake it is now a mistake the actor nudges.
                || Ok(concludes(serde_json::json!({"description": "fixed"}))),
            ),
        );
        let (f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let run = wait_for_run(&journal, id, |r| {
            r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
        })
        .await;
        let step_agent = run.steps[0].agent;

        // A second actor over the same journal: nothing is resident, which is
        // every read after an idle offload or a restart.
        f.node.restart().await;
        let reloaded = f.node.session(id);

        let log = reloaded
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::ReadLog {
                    agent_id: Some(step_agent.to_string()),
                    after: None,
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a cold step must resolve to its own agent");
        assert!(
            !log.entries.is_empty(),
            "the step's transcript is what the step page shows"
        );
    }

    // ---- invoked runs: any agent can start a workflow mid-session ----

    use crate::sessions::run_forest::{MAX_DEPTH, MAX_LIVE_RUNS, RunId};
    use crate::sessions::workflow::{WorkflowRunSpec, WorkflowRunStatus};

    /// A one-step graph whose prompt decides how [`NestedRunsProvider`]
    /// answers its step.
    fn single_step_graph(workflow: &str, prompt: &str) -> Arc<WorkflowRunSpec> {
        use crate::sessions::workflow::WorkflowStepSpec;
        Arc::new(WorkflowRunSpec {
            workflow: workflow.into(),
            start: "ship".into(),
            steps: vec![WorkflowStepSpec {
                name: "ship".into(),
                agent: "shipper".into(),
                prompt: prompt.into(),
                outcomes: crate::sessions::workflow::default_outcomes(),
                fields: Vec::new(),
                interactive: false,
                transitions: vec![],
                settings: agent_settings_fixture(),
            }],
            input: "go".into(),
            max_steps: 10,
        })
    }

    /// Routes by prompt marker, so one provider serves a whole nesting tree:
    /// `Triage it.`/`HANG` hold an agent mid-turn for as long as the test
    /// looks at it, `SHIP` concludes a step, everything else is plain text.
    struct NestedRunsProvider;

    #[async_trait]
    impl horsie_agentcore::LlmProvider for NestedRunsProvider {
        fn model_id(&self) -> &str {
            "mock"
        }

        async fn complete(
            &self,
            request: horsie_agentcore::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            let says = |needle: &str| {
                request.messages.iter().any(|m| {
                    m.parts.iter().any(|p| {
                        matches!(p, horsie_agentcore::ContentPart::Text(t) if t.text.contains(needle))
                    })
                })
            };
            if says("Triage it.") || says("HANG") {
                std::future::pending::<()>().await;
            }
            if says("SHIP") {
                return Ok(concludes(serde_json::json!({"description": "shipped"})));
            }
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "sub answer".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    async fn invoke(
        session: &SessionRef,
        parent: Uuid,
        graph: Arc<WorkflowRunSpec>,
    ) -> Result<Uuid, String> {
        session
            .ask(|reply| {
                SessionCommand::Run(RunCommand::Create {
                    parent,
                    graph,
                    reply,
                })
            })
            .await
            .unwrap()
    }

    /// The feature, end to end: the main agent invokes a workflow, its step
    /// runs and concludes, and the run's report is delivered back into the
    /// invoker's own conversation — through the same owed-delivery rule a
    /// subagent's report takes.
    #[tokio::test]
    async fn an_invoked_run_executes_and_reports_to_its_invoker() {
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        let run = invoke(
            &session,
            id,
            single_step_graph("deploy", "SHIP the artifact."),
        )
        .await
        .expect("main may invoke a workflow");

        wait_for_state(&journal, id, "the invoked run to finish and report", |s| {
            s.forest
                .workflow(RunId(run))
                .is_some_and(|w| w.run.status == WorkflowRunStatus::Finished && w.notified)
        })
        .await;
        // Never the session's own status: an invoked run's phase is its own.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_ne!(
            state.status(),
            crate::sessions::spec::SessionStatus::Finished,
            "a conversation does not finish because a run it invoked did"
        );
        let texts = wait_for_subagent_text(&session, |texts| {
            texts
                .iter()
                .any(|t| t.contains("workflow deploy") && t.contains("shipped"))
        })
        .await;
        assert!(
            texts
                .iter()
                .any(|t| t.contains("[subagent \"workflow deploy\" completed]")
                    && t.contains("shipped")),
            "the invoker hears the run's result: {texts:?}"
        );
    }

    /// Nesting: a workflow's own step invokes another workflow, and the report
    /// is owed to the *step agent* — the run forest's one parent edge, again.
    #[tokio::test]
    async fn a_workflow_step_invokes_a_workflow_and_is_owed_the_report() {
        let (_f, session, id, journal) =
            spawn_run_with_provider(Arc::new(NestedRunsProvider)).await;
        let root = wait_for_run(&journal, id, |r| r.current().is_some()).await;
        let step_agent = root.steps[0].agent;

        let nested = invoke(
            &session,
            step_agent,
            single_step_graph("deploy", "SHIP the artifact."),
        )
        .await
        .expect("a step may invoke a workflow");

        wait_for_state(&journal, id, "the nested run to finish and report", |s| {
            s.forest
                .workflow(RunId(nested))
                .is_some_and(|w| w.run.status == WorkflowRunStatus::Finished && w.notified)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state
                .forest
                .owner_of_agent(nested)
                .map(|(id, _)| id)
                .or_else(|| state.forest.entry(RunId(nested)).map(|_| RunId(nested))),
            Some(RunId(nested)),
        );
        assert_eq!(
            state.forest.entry(RunId(nested)).unwrap().parent,
            Some(step_agent),
            "the run reports to the step that invoked it"
        );
        // And the root run is untouched by its child finishing.
        assert_eq!(
            state.forest.root_workflow().unwrap().1.run.status,
            WorkflowRunStatus::Running
        );
    }

    /// Sibling runs progress concurrently: one blocked run does not stop
    /// another from finishing.
    #[tokio::test]
    async fn sibling_invoked_runs_progress_independently() {
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        let stuck = invoke(&session, id, single_step_graph("slow", "HANG forever."))
            .await
            .expect("first run");
        let quick = invoke(&session, id, single_step_graph("quick", "SHIP it."))
            .await
            .expect("second run");

        wait_for_state(&journal, id, "the quick run to finish", |s| {
            s.forest
                .workflow(RunId(quick))
                .is_some_and(|w| w.run.status == WorkflowRunStatus::Finished)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.forest.workflow(RunId(stuck)).unwrap().run.status,
            WorkflowRunStatus::Running,
            "the blocked sibling is still going, independently"
        );
    }

    /// The recursion bound: an invocation is a delegation edge like a spawn,
    /// so a chain four deep refuses the fifth — which is what stops a workflow
    /// that invokes itself from running away.
    #[tokio::test]
    async fn invoking_beyond_the_delegation_depth_is_refused() {
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        // A chain of subagents built by seeding events would race the resident
        // roster; spawn them for real instead, held open by HANG tasks.
        let mut parent = id;
        for _ in 0..MAX_DEPTH {
            let child = session
                .ask(|reply| {
                    SessionCommand::SubAgent(SubAgentCommand::Spawn {
                        caller: parent,
                        label: "link".into(),
                        task: "HANG until stopped".into(),
                        agent_type: None,
                        reply,
                    })
                })
                .await
                .unwrap()
                .unwrap();
            wait_for_tree(&journal, id, |t| t.sub(child).is_some()).await;
            parent = child;
        }
        let err = invoke(&session, parent, single_step_graph("deep", "SHIP it."))
            .await
            .expect_err("the fifth delegation level is refused");
        assert_eq!(err, format!("max delegation depth {MAX_DEPTH} reached"));
    }

    #[tokio::test]
    async fn invoking_beyond_the_live_run_cap_is_refused() {
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        for n in 0..MAX_LIVE_RUNS {
            let run = invoke(
                &session,
                id,
                single_step_graph(&format!("slow-{n}"), "HANG forever."),
            )
            .await
            .expect("live runs under the cap start");
            wait_for_state(&journal, id, "the run to be live", |s| {
                s.forest.workflow(RunId(run)).is_some()
            })
            .await;
        }
        let err = invoke(&session, id, single_step_graph("one-too-many", "SHIP it."))
            .await
            .expect_err("the ninth live run is refused");
        assert_eq!(
            err,
            format!("{MAX_LIVE_RUNS} workflow runs already live in this session")
        );
    }

    #[tokio::test]
    async fn invoking_from_an_unknown_agent_is_refused() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        let err = invoke(
            &session,
            Uuid::new_v4(),
            single_step_graph("orphan", "SHIP it."),
        )
        .await
        .expect_err("an unknown caller is refused");
        assert_eq!(err, "caller is not a known agent");
    }

    /// Persist-then-start, replayed: a crash between the `RunCreated` write
    /// and anything else leaves a pending run, and loading is what starts it.
    #[tokio::test]
    async fn a_run_created_just_before_a_crash_starts_at_load() {
        let (f, session, id, journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        drop(session);
        let run = Uuid::new_v4();
        let session2 = seed_session(
            &f,
            id,
            actor_spec_fixture(),
            &[SessionDomainEvent::RunCreated {
                at_ms: 1,
                id: run,
                parent: id,
                graph: single_step_graph("deploy", "SHIP the artifact."),
            }],
        )
        .await;
        wait_for_state(&journal, id, "the recovered run to finish", |s| {
            s.forest
                .workflow(RunId(run))
                .is_some_and(|w| w.run.status == WorkflowRunStatus::Finished)
        })
        .await;
        let _ = session2;
    }

    /// At-least-once delivery for run reports: a crash after `RunFinished` and
    /// before `RunNotified` leaves the report owed, and the next boundary — a
    /// person acting — re-delivers it.
    #[tokio::test]
    async fn a_run_finished_but_unreported_before_a_crash_is_redelivered() {
        let (f, session, id, journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        drop(session);
        let run = Uuid::new_v4();
        let graph = single_step_graph("deploy", "SHIP the artifact.");
        let step_agent = WorkflowRunSpec::step_agent_id(run, 0);
        let session2 = seed_session(
            &f,
            id,
            actor_spec_fixture(),
            &[
                SessionDomainEvent::RunCreated {
                    at_ms: 1,
                    id: run,
                    parent: id,
                    graph,
                },
                SessionDomainEvent::StepStarted {
                    at_ms: 2,
                    run,
                    index: 0,
                    step: "ship".into(),
                    agent: step_agent,
                    attempt: 1,
                    from: None,
                    via: None,
                    input: "SHIP the artifact.".into(),
                },
                SessionDomainEvent::StepConcluded {
                    at_ms: 3,
                    run,
                    index: 0,
                    output: serde_json::json!({"outcome": "success", "description": "shipped"}),
                },
                SessionDomainEvent::RunFinished {
                    at_ms: 4,
                    run,
                    output: serde_json::json!({"outcome": "success", "description": "shipped"}),
                },
            ],
        )
        .await;
        // Loading starts nothing; the user's next action is the boundary.
        send(&session2, "hello again").await;
        wait_for_state(&journal, id, "the stranded report re-delivered", |s| {
            s.forest.workflow(RunId(run)).is_some_and(|w| w.notified)
        })
        .await;
        let texts = wait_for_subagent_text(&session2, |texts| {
            texts.iter().any(|t| t.contains("workflow deploy"))
        })
        .await;
        assert!(
            texts.iter().any(|t| t.contains("shipped")),
            "the report survives the crash: {texts:?}"
        );
    }

    /// Stopping a subagent ends its delegation: the workflow it invoked is
    /// cancelled with it, in-flight step and all, and reports the stop.
    #[tokio::test]
    async fn stopping_a_subagent_cancels_the_workflow_it_invoked() {
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        let sub = spawn_sub(&session, "lead", "HANG while delegating").await;
        wait_for_tree(&journal, id, |t| t.sub(sub).is_some()).await;
        let run = invoke(&session, sub, single_step_graph("slow", "HANG forever."))
            .await
            .expect("a subagent may invoke a workflow");
        wait_for_state(&journal, id, "the nested run's step in flight", |s| {
            s.forest
                .workflow(RunId(run))
                .is_some_and(|w| w.run.current().is_some())
        })
        .await;

        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::Stop {
                    agent_id: sub.to_string(),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a working subagent is stoppable");

        let state = wait_for_state(&journal, id, "the cascade to land", |s| {
            s.forest
                .workflow(RunId(run))
                .is_some_and(|w| w.run.status == WorkflowRunStatus::Failed)
        })
        .await;
        let w = state.forest.workflow(RunId(run)).unwrap();
        assert_eq!(
            w.run.error.as_deref(),
            Some(crate::sessions::run_forest::STOPPED_ERROR)
        );
        assert_eq!(
            w.run.steps[0].status,
            crate::sessions::workflow::StepStatus::Cancelled,
            "the in-flight step is cancelled, not left running for ever"
        );
    }

    /// Retrying a step supersedes the old execution, and the old execution's
    /// outstanding children go with it — a result addressed to a replaced
    /// step would otherwise arrive at something the run has moved past.
    #[tokio::test]
    async fn retrying_a_step_cancels_the_superseded_steps_invoked_run() {
        let (_f, session, id, journal) =
            spawn_run_with_provider(Arc::new(NestedRunsProvider)).await;
        let root = wait_for_run(&journal, id, |r| r.current().is_some()).await;
        let step_agent = root.steps[0].agent;
        let nested = invoke(
            &session,
            step_agent,
            single_step_graph("slow", "HANG forever."),
        )
        .await
        .expect("the step invokes a workflow");
        wait_for_state(&journal, id, "the nested run's step in flight", |s| {
            s.forest
                .workflow(RunId(nested))
                .is_some_and(|w| w.run.current().is_some())
        })
        .await;

        session
            .ask(|reply| SessionCommand::Run(RunCommand::RetryStep { index: 0, reply }))
            .await
            .unwrap()
            .expect("a step in flight is retryable");

        let state = wait_for_state(&journal, id, "the superseded child cancelled", |s| {
            s.forest
                .workflow(RunId(nested))
                .is_some_and(|w| w.run.status == WorkflowRunStatus::Failed)
        })
        .await;
        assert_eq!(
            state
                .forest
                .workflow(RunId(nested))
                .unwrap()
                .run
                .error
                .as_deref(),
            Some(crate::sessions::run_forest::STOPPED_ERROR)
        );
    }

    /// `workflow_status` answers the invoker and its ancestors; a stranger —
    /// or an id that names nothing — gets the same refusal, so neither
    /// confirms the run exists.
    #[tokio::test]
    async fn run_status_is_visible_to_the_invoker_and_nobody_else() {
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(NestedRunsProvider)).await;
        let outsider = spawn_sub(&session, "bystander", "HANG around").await;
        wait_for_tree(&journal, id, |t| t.sub(outsider).is_some()).await;
        let run = invoke(&session, id, single_step_graph("slow", "HANG forever."))
            .await
            .expect("a run");

        let status = |caller: Uuid| {
            let session = session.clone();
            async move {
                session
                    .ask(move |reply| {
                        SessionCommand::Run(RunCommand::Status { caller, run, reply })
                    })
                    .await
                    .unwrap()
            }
        };
        let rendered = status(id).await.expect("the invoker sees its run");
        assert!(rendered.contains("workflow \"slow\""), "{rendered}");
        assert!(
            status(outsider).await.is_err(),
            "a sibling must not see someone else's run"
        );
    }
}
