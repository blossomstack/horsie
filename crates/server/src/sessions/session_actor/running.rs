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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    //! The graph through the actor: what starts a run, how a transition
    //! routes, and what a retry appends.
    use super::super::testing::*;
    use super::super::*;
    use crate::sessions::session_actor::testing::seed_session;
    use crate::sessions::spec::SessionStatus;

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
                || Ok(concludes(serde_json::json!({"description": "fixed"}))),
            ),
        );
        let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
        let state = wait_for_state(&journal, id, "the run to finish", |s| {
            root_run_of(s)
                .is_some_and(|r| r.status == crate::sessions::workflow::WorkflowRunStatus::Finished)
        })
        .await;
        assert_eq!(
            state.status(),
            SessionStatus::Finished,
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
        // Each step is its own agent, derived from the run and the index.
        assert_eq!(
            AgentId(run.steps[0].agent),
            super::step_agent_id(RunnerId(id), 0)
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
    /// than the step failed outright.
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
    /// stuck: the timer will wake it.
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
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let run = root_run_of(&state).expect("the run exists");
        assert_eq!(
            run.steps[0].status,
            crate::sessions::workflow::StepStatus::Running,
            "the step is still running, waiting on its timer"
        );
    }

    /// Submitting says the work is done, which makes an armed timer moot.
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
        // nothing.
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

    /// A run has no first message to hold it back — it starts itself at load.
    /// So it needs the same wait a conversation gets, and for the same reason:
    /// the step would ask for a runtime nobody had been told to build.
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
            s.status() == SessionStatus::Provisioning
        })
        .await;
        let held = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(
            root_run_of(&held).is_none_or(|r| r.steps.is_empty()),
            "no step may start before the runtime it would run on"
        );

        f.agent.release_creates();
        wait_for_run(&journal, id, |r| !r.steps.is_empty()).await;
    }

    /// A step asks, is answered *without* the caller naming it, and the run
    /// carries on: a run has no main agent, so an unaddressed answer means the
    /// step in flight.
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
    /// the step's entry stayed `Running`, so `current()` never cleared and
    /// nothing started ever again.
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

    /// A step the process died inside is suspended at load, not resumed: how
    /// far it got is unknowable and its effect on the shared workspace with
    /// it.
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
            &[SessionEvent::Runner {
                id: RunnerId(id),
                at_ms: 0,
                event: RunnerEvent::StepStarted {
                    index: 0,
                    step: "triage".into(),
                    agent: super::step_agent_id(RunnerId(id), 0),
                    attempt: 1,
                    from: None,
                    via: None,
                    input: "Triage it.".into(),
                },
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

    /// A finished run's step transcript survives the session unloading: a
    /// reloaded run holds an empty roster, and every agent-scoped read spawns
    /// the cold agent on demand.
    #[tokio::test]
    async fn a_cold_steps_transcript_is_still_readable_after_a_reload() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(concludes(serde_json::json!({"outcome": "p0"})))]).then_repeating_with(
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
}
