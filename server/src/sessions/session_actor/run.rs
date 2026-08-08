//! The workflow graph, when this session is a run.
//!
//! Reads the run log, evaluates the transition out of the last concluded step,
//! and decides the next step, the run's end, or its failure. Appends rather than
//! replaces: a loop back onto a step and a retry of one are both new entries,
//! which is what keeps the log replayable and the graph projection lossless.
//!
//! Silent when `state.run` is `None`. That check, rather than a branch chosen at
//! construction, is the whole of what makes this component inert in a
//! conversation.

use super::component::{ActionCx, Component};
use super::context::SessionAgentKind;
use super::{
    AgentAction, AgentKey, AgentPlan, CommandEffect, RunCommand, SessionActor, SessionCommand,
    SessionDomainEvent, SessionState, TurnEnd,
};
use crate::sessions::orchestrator::StepStart;
use crate::sessions::spec::SessionStatus;
use crate::sessions::workflow::WorkflowRunState;
use horsie_actor::ActorContext;
use horsie_actor::ActorRef;
use horsie_actor::EventSourcedActor;
use horsie_models::now_ms;
use horsie_workflow::{AgentCommand, Incoming};
use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

/// WorkflowRun.
pub(super) struct WorkflowRun;

impl WorkflowRun {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: RunCommand,
        ctx: &ActorContext<SessionActor>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            RunCommand::State { reply } => {
                let _ = reply.send(state.run.clone());
                CommandEffect::none()
            }
            RunCommand::Advance => CommandEffect::persist(actor.flush_then_drain(state, ctx).await),
            RunCommand::RetryStep { index, reply } => {
                actor.on_retry_step(state, index, reply, ctx).await
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
        ctx: &ActorContext<Self>,
    ) -> Vec<SessionDomainEvent> {
        let StepStart {
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
        let Some(actor) = self.spawn_step_agent(ctx, state, agent, &step) else {
            return vec![SessionDomainEvent::RunFailed {
                at_ms: now_ms(),
                error: format!("step '{step}' is no longer in this workflow"),
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
            return vec![SessionDomainEvent::RunFailed {
                at_ms: now_ms(),
                error: format!("step '{step}' could not be started"),
            }];
        }
        self.report(SessionStatus::Running).await;
        vec![SessionDomainEvent::StepStarted {
            at_ms: now_ms(),
            index,
            step,
            agent,
            attempt,
            from,
            via,
            input,
        }]
    }

    /// The run reached a terminal step and succeeded.
    pub(super) async fn finish_run(&mut self, output: Value) -> Vec<SessionDomainEvent> {
        self.report(SessionStatus::Idle).await;
        vec![SessionDomainEvent::RunFinished {
            at_ms: now_ms(),
            output,
        }]
    }

    /// The run cannot continue — no transition matched, or a step failed.
    pub(super) async fn fail_run(&mut self, error: String) -> Vec<SessionDomainEvent> {
        self.report(SessionStatus::Failed {
            reason: error.clone(),
        })
        .await;
        vec![SessionDomainEvent::RunFailed {
            at_ms: now_ms(),
            error,
        }]
    }
    /// Re-run one execution from the log.
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
        reply: oneshot::Sender<Result<(), String>>,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        let Some(run) = state.run.as_ref() else {
            let _ = reply.send(Err("this session is not a workflow run".into()));
            return CommandEffect::none();
        };
        let Some(target) = run.get(index).cloned() else {
            let _ = reply.send(Err(format!("no step execution at index {index}")));
            return CommandEffect::none();
        };
        let mut events = Vec::new();
        // Cancel whatever is in flight first, so the retry is the only writer.
        if let Some(current) = run.current() {
            if let Some(step) = run.get(current) {
                self.cancel_agent(AgentKey::Step(step.agent)).await;
            }
            events.push(SessionDomainEvent::StepCancelled {
                at_ms: now_ms(),
                index: current,
            });
        }
        let mut next = state.clone();
        for e in &events {
            next = SessionActor::apply_event(next, e.clone());
        }
        let new_index = next
            .run
            .as_ref()
            .map(|r| r.steps.len() as u32)
            .unwrap_or_default();
        let attempt = next
            .run
            .as_ref()
            .map(|r| r.attempts_of(&target.step) + 1)
            .unwrap_or(1);
        let _ = reply.send(Ok(()));
        events.extend(
            self.start_step(
                StepStart {
                    index: new_index,
                    step: target.step.clone(),
                    agent: crate::sessions::workflow::WorkflowRunSpec::step_agent_id(
                        self.id, new_index,
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
        index: u32,
        end: TurnEnd,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        let (events, advance) = match end {
            TurnEnd::Concluded { output } => (
                vec![SessionDomainEvent::StepConcluded {
                    at_ms: now_ms(),
                    index,
                    output,
                }],
                true,
            ),
            TurnEnd::Asked => {
                self.report(SessionStatus::AwaitingInput).await;
                (
                    vec![SessionDomainEvent::AskRecorded { at_ms: now_ms() }],
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
            TurnEnd::Failed { error, .. } => {
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                (
                    vec![SessionDomainEvent::StepFailed {
                        at_ms: now_ms(),
                        index,
                        error,
                    }],
                    false,
                )
            }
            TurnEnd::Parked => {
                let error = "step parked; timers are not supported in workflows".to_string();
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                (
                    vec![SessionDomainEvent::StepFailed {
                        at_ms: now_ms(),
                        index,
                        error,
                    }],
                    false,
                )
            }
        };
        match advance {
            true => self.persist_and_advance(state, events, ctx).await,
            false => CommandEffect::persist(events),
        }
    }
    /// Spawn the agent for one execution of a workflow step.
    ///
    /// Differs from a subagent in three ways, all of them the point: it runs
    /// with its *own* preset's settings rather than the session's, it carries
    /// the step's output schema so `conclude` is typed, and it is keyed as a
    /// step so it roots its own subagent tree.
    pub(super) fn spawn_step_agent(
        &mut self,
        ctx: &ActorContext<Self>,
        state: &SessionState,
        agent_id: Uuid,
        step_name: &str,
    ) -> Option<ActorRef<AgentCommand>> {
        let run_spec = self.spec.workflow.clone()?;
        let step = run_spec.step(step_name)?.clone();
        Some(
            self.spawn_agent(
                ctx,
                state,
                AgentPlan {
                    kind: SessionAgentKind::Step(agent_id),
                    // A step runs under its own preset, not the session's.
                    settings: step.settings.clone(),
                    step_output_schema: step.output_schema.clone(),
                    agent_type: None,
                    // Deliberately none. A step's terminal tool is `conclude`,
                    // synthesized from its output schema; naming `ask_user`
                    // beside it would stop the loop treating `conclude` as
                    // terminal, so it would try to *execute* it, get "the
                    // conclude tool is terminal and is not executed" back, and
                    // keep going. A step asks through `conclude(kind=ask)`.
                    handoff_tool: None,
                },
            )
            .actor,
        )
    }
}

impl Component for WorkflowRun {
    /// The next step, the run's end, or its failure. Silent when this session is
    /// not a run — that check, and not a branch chosen at construction, is what
    /// makes this component inert in a conversation.
    fn actions(cx: &ActionCx<'_>, state: &SessionState) -> Vec<AgentAction> {
        let Some(run_spec) = cx.spec.workflow.clone() else {
            return Vec::new();
        };
        crate::sessions::workflow::WorkflowOrchestrator::new(cx.id, run_spec).step_actions(state)
    }

    /// A run is created and then left to begin by itself, with no first message
    /// to trigger it. This is the one place a session starts work at load.
    ///
    /// Gated on the *spec*: a conversation also has no run state, and reading
    /// only the state would advance one — which, for a session holding a
    /// subagent result nobody has collected, silently starts a turn at load.
    fn on_load(cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
        cx.spec.workflow.as_ref()?;
        let unstarted = match &state.run {
            None => true,
            Some(run) => run.status == crate::sessions::workflow::WorkflowRunStatus::Pending,
        };
        unstarted.then_some(SessionCommand::Run(RunCommand::Advance))
    }

    fn busy(state: &SessionState) -> bool {
        state.run.as_ref().is_some_and(|r| r.current().is_some())
    }

    /// The run log. Appended, never replaced — a loop back onto a step and a
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
            SessionDomainEvent::StepStarted {
                at_ms,
                step,
                agent,
                attempt,
                from,
                via,
                input,
                ..
            } => {
                // The first step is what turns the state into a run:
                // `initial_state` is static and cannot see the spec, so the mode
                // is established by the log rather than at construction.
                // The first step is what turns this state into a run:
                // `initial_state` is static and cannot see the spec, so the run
                // is established by the log rather than at construction.
                let run = state.run.get_or_insert_with(WorkflowRunState::default);
                {
                    run.apply_started(step, agent, attempt, from, via, input, at_ms);
                }
                state.status = SessionStatus::Running;
                state.last_error = None;
            }
            SessionDomainEvent::StepConcluded {
                at_ms,
                index,
                output,
            } => {
                if let Some(run) = state.run.as_mut() {
                    run.apply_concluded(index, output, at_ms);
                }
            }
            SessionDomainEvent::StepFailed {
                at_ms,
                index,
                error,
            } => {
                if let Some(run) = state.run.as_mut() {
                    run.apply_step_failed(index, error.clone(), at_ms);
                    run.apply_failed(error.clone());
                }
                state.status = SessionStatus::Failed {
                    reason: error.clone(),
                };
                state.last_error = Some(error);
            }
            SessionDomainEvent::StepCancelled { at_ms, index } => {
                if let Some(run) = state.run.as_mut() {
                    run.apply_cancelled(index, at_ms);
                }
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::RunFinished { output, .. } => {
                if let Some(run) = state.run.as_mut() {
                    run.apply_finished(output);
                }
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::RunFailed { error, .. } => {
                if let Some(run) = state.run.as_mut() {
                    run.apply_failed(error.clone());
                }
                state.status = SessionStatus::Failed {
                    reason: error.clone(),
                };
                state.last_error = Some(error);
            }
            other => unreachable!("WorkflowRun was handed {other:?}"),
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
    use super::*;

    use horsie_agentcore::LlmProvider;

    use std::sync::Arc;
    use uuid::Uuid;

    /// The whole point: a run starts itself, its first step's output picks the
    /// branch, and the branch's step ends the run.
    #[tokio::test]
    async fn a_run_starts_itself_and_routes_on_its_first_steps_output() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"severity": "p0"})))]).then_repeating_with(
                || {
                    Ok(horsie_agentcore::CompletionResponse {
                        parts: vec![horsie_agentcore::ContentPart::Text(
                            horsie_agentcore::TextPart {
                                text: "fixed".to_string(),
                            },
                        )],
                        stop_reason: horsie_agentcore::StopReason::EndTurn,
                        usage: horsie_agentcore::Usage::without_cache(1, 1),
                    })
                },
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
        assert_eq!(
            run.steps[1].via.as_deref(),
            Some("output.severity == \"p0\"")
        );
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
            Script::of([Ok(concludes(serde_json::json!({"severity": "p2"})))]).then_repeating_with(
                || {
                    Ok(horsie_agentcore::CompletionResponse {
                        parts: vec![horsie_agentcore::ContentPart::Text(
                            horsie_agentcore::TextPart {
                                text: "filed".to_string(),
                            },
                        )],
                        stop_reason: horsie_agentcore::StopReason::EndTurn,
                        usage: horsie_agentcore::Usage::without_cache(1, 1),
                    })
                },
            ),
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

    /// Retrying appends an attempt rather than replacing one, so the earlier
    /// attempt stays readable and the graph can stack them.
    #[tokio::test]
    async fn retrying_a_step_appends_an_attempt_on_the_same_edge() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"severity": "p0"})))]).then_repeating_with(
                || {
                    Ok(horsie_agentcore::CompletionResponse {
                        parts: vec![horsie_agentcore::ContentPart::Text(
                            horsie_agentcore::TextPart {
                                text: "fixed".to_string(),
                            },
                        )],
                        stop_reason: horsie_agentcore::StopReason::EndTurn,
                        usage: horsie_agentcore::Usage::without_cache(1, 1),
                    })
                },
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
            Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
        );
        let id = Uuid::new_v4();
        let mut spec = actor_spec_fixture();
        spec.origin = crate::sessions::spec::SessionOrigin::Workflow {
            workflow: "fix-bug".into(),
        };
        spec.workflow = Some(Arc::new(run_spec_fixture("the build is red")));
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                spec,
                f.deps.clone(),
                spawn_deaf_supervisor(),
                crate::sessions::Positions::default(),
            ),
            journal.clone(),
        );
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();

        wait_for_state(&journal, id, "a run holding at its create", |s| {
            s.status == SessionStatus::Provisioning
        })
        .await;
        let held = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(
            held.run.as_ref().is_none_or(|r| r.steps.is_empty()),
            "no step may start before the runtime it would run on"
        );

        f.agent.release_creates();
        wait_for_run(&journal, id, |r| !r.steps.is_empty()).await;
    }
}
