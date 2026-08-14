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
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::addressing::SessionInbox;
use crate::sessions::orchestrator::StepStart;
use crate::sessions::spec::SessionStatus;
use crate::sessions::workflow::WorkflowRunState;
use horsie_actor::ActorContext;
use horsie_actor::ActorRef;
use horsie_actor::EventSourcedActor;
use horsie_actor::ReplyTo;
use horsie_models::now_ms;
use serde_json::Value;
use uuid::Uuid;

/// WorkflowRun.
pub(super) struct WorkflowRun;

impl WorkflowRun {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: RunCommand,
        ctx: &ActorContext<SessionInbox>,
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
            RunCommand::ReconcileInterrupted => {
                let Some(index) = state.run.as_ref().and_then(WorkflowRunState::current) else {
                    return CommandEffect::none();
                };
                CommandEffect::persist(vec![SessionDomainEvent::StepCancelled {
                    at_ms: now_ms(),
                    index,
                }])
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
        vec![SessionDomainEvent::RunFinished {
            at_ms: now_ms(),
            output,
        }]
    }

    /// The run cannot continue — no transition matched, or a step failed.
    pub(super) async fn fail_run(&mut self, error: String) -> Vec<SessionDomainEvent> {
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
        reply: ReplyTo<Result<(), String>>,
        ctx: &ActorContext<SessionInbox>,
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
        ctx: &ActorContext<SessionInbox>,
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
            TurnEnd::Failed { error, .. } => (
                vec![SessionDomainEvent::StepFailed {
                    at_ms: now_ms(),
                    index,
                    error,
                }],
                false,
            ),
            // A step that armed a timer, or is waiting on subagents it spawned,
            // ends its turn without finishing — and that is not the step
            // ending. It stays running until whatever it is waiting for wakes
            // it, exactly as a parked question does. This used to fail the run
            // outright, which made a step that suspended itself deliberately
            // indistinguishable from one that crashed.
            TurnEnd::Parked => (Vec::new(), false),
            // A step the process died inside is suspended by
            // `WorkflowRun::on_load`, which is the state a retry can move.
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
    /// Spawn the agent for one execution of a workflow step.
    ///
    /// Differs from a subagent in three ways, all of them the point: it runs
    /// with its *own* preset's settings rather than the session's, it carries
    /// what the step promises to return so `submit_result` is typed, and it is
    /// keyed as a step so it roots its own subagent tree.
    pub(super) fn spawn_step_agent(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        agent_id: Uuid,
        step_name: &str,
    ) -> Option<ActorRef<AgentCommand>> {
        let run_spec = self.spec().workflow_run().cloned()?;
        let step = run_spec.step(step_name)?.clone();
        // A step runs under its own preset. Resolved here from the run
        // snapshot rather than through [`SessionActor::effective_settings`]:
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

impl Component for WorkflowRun {
    /// The next step, the run's end, or its failure. Silent when this session is
    /// not a run — that check, and not a branch chosen at construction, is what
    /// makes this component inert in a conversation.
    fn actions(cx: &ActionCx<'_>, state: &SessionState) -> Vec<AgentAction> {
        let Some(run_spec) = cx.spec.workflow_run().cloned() else {
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
    ///
    /// A step left in flight is the other thing recovery finds, and it is not
    /// resumed: how far it got is unknowable, and its effect on the shared
    /// workspace with it. It is suspended instead, which is what makes a retry
    /// the person's decision. Without this the entry stayed `Running`, so
    /// `current()` never cleared and the run started nothing ever again.
    fn on_load(cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
        cx.spec.workflow_run()?;
        match &state.run {
            None => Some(SessionCommand::Run(RunCommand::Advance)),
            Some(run) if run.current().is_some() => {
                Some(SessionCommand::Run(RunCommand::ReconcileInterrupted))
            }
            Some(run) if run.status == crate::sessions::workflow::WorkflowRunStatus::Pending => {
                Some(SessionCommand::Run(RunCommand::Advance))
            }
            Some(_) => None,
        }
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
                // Not `Idle`: a run that ran to completion and one that stopped
                // part-way both rest, and telling them apart is the whole
                // reason to look at a list of past runs.
                state.status = SessionStatus::Finished;
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
            s.run
                .as_ref()
                .is_some_and(|r| r.status == crate::sessions::workflow::WorkflowRunStatus::Finished)
        })
        .await;
        assert_eq!(
            state.status,
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
            .run
            .expect("the run exists");
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
}
