//! One workflow run.
//!
//! Wraps every step agent the run executes: listens to their lifecycles,
//! folds each conclusion into the run log, and decides — purely, from the log
//! and the graph snapshotted at creation — what starts next. The imperative
//! half (spawning the step agent, enqueueing its input) is the actor's;
//! everything here is a decision.
//!
//! Instantiable, which is the point: the session-root run and a run an agent
//! invoked mid-session are the same runner with a different parent, and the
//! session holds as many as its work created.

use crate::sessions::spec::SessionSpec;
use crate::sessions::workflow::{
    OUTCOME_FIELD, StepStatus, WorkflowRunStatus, compose_step_input, next_transition,
    output_as_input,
};
use serde_json::Value;
use uuid::Uuid;

use super::super::context::StepResultDef;
use super::super::types::TurnEnd;
use super::RunnerBehavior;
use super::action::{OutcomeDecision, Repair, RunnerAction, StepStart};
use super::deliver;
use super::event::{RecordedEnd, RunnerEvent, SessionEvent};
use super::ids::{AgentId, RunnerId};
use super::role::{AgentRole, STEP_PROMPT_SUFFIX, StopHookKind, TitleScope};
use super::state::{RunnerState, SessionState, WorkflowState};

pub(crate) struct WorkflowRunner {
    pub id: RunnerId,
}

/// The agent that runs one execution of one step: derived, never minted, so a
/// pure replay reconstructs identical ids — and derived from the *run* rather
/// than the session, so two runs' step agents can never collide.
pub(crate) fn step_agent_id(run: RunnerId, index: u32) -> AgentId {
    AgentId(Uuid::new_v5(&run.0, format!("step:{index}").as_bytes()))
}

impl WorkflowRunner {
    fn workflow<'a>(&self, state: &'a SessionState) -> Option<&'a WorkflowState> {
        match &state.record(self.id)?.state {
            RunnerState::Workflow(w) => Some(w),
            RunnerState::Main(_) | RunnerState::Sub(_) | RunnerState::Fork(_) => None,
        }
    }

    /// The action that starts `step`, coming out of `from`.
    fn start(
        &self,
        w: &WorkflowState,
        step_name: &str,
        from: Option<u32>,
        via: Option<String>,
        incoming: &str,
        from_step: Option<&str>,
    ) -> RunnerAction {
        let index = w.run.steps.len() as u32;
        let prompt = w
            .graph
            .step(step_name)
            .map(|s| s.prompt.as_str())
            .unwrap_or_default();
        RunnerAction::StartStep {
            run: self.id,
            start: StepStart {
                index,
                step: step_name.to_string(),
                agent: step_agent_id(self.id, index),
                attempt: w.run.attempts_of(step_name) + 1,
                from,
                via,
                input: compose_step_input(prompt, from_step, incoming),
            },
        }
    }

    /// What the graph wants started: the next step, the run's end, or its
    /// failure. Knows nothing about subagents.
    fn step_actions(&self, w: &WorkflowState) -> Vec<RunnerAction> {
        // A step in flight, a park, a suspension and a terminal run all mean
        // the same thing here: nothing starts by itself. Only a retry moves a
        // suspended run, and only an answer moves a parked one.
        if w.run.status.is_terminal()
            || matches!(
                w.run.status,
                WorkflowRunStatus::Suspended | WorkflowRunStatus::AwaitingInput
            )
            || w.run.current().is_some()
        {
            return Vec::new();
        }
        // A loop whose condition never flips would otherwise run forever. The
        // budget is checked before starting, so the log holds exactly the
        // executions that ran.
        if w.run.steps.len() as u32 >= w.graph.max_steps {
            return vec![RunnerAction::FailRun {
                run: self.id,
                error: format!("step budget exhausted after {} steps", w.graph.max_steps),
            }];
        }
        let Some((index, last)) = w.run.last() else {
            // Nothing has run: begin at the start step.
            if w.graph.step(&w.graph.start).is_none() {
                return vec![RunnerAction::FailRun {
                    run: self.id,
                    error: format!("start step '{}' is not in this workflow", w.graph.start),
                }];
            }
            return vec![self.start(w, &w.graph.start.clone(), None, None, &w.graph.input, None)];
        };
        // Only a concluded step decides anything. A failed one already failed
        // the run; a cancelled one waits for a retry.
        if last.status != StepStatus::Concluded {
            return Vec::new();
        }
        let output = last.output.clone().unwrap_or(Value::Null);
        let Some(step) = w.graph.step(&last.step) else {
            return vec![RunnerAction::FailRun {
                run: self.id,
                error: format!("step '{}' is no longer in this workflow", last.step),
            }];
        };
        let outcome = output
            .get(OUTCOME_FIELD)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match next_transition(&step.transitions, &outcome) {
            // No transition matched: this step is terminal, and its result is
            // the run's.
            None => vec![RunnerAction::FinishRun {
                run: self.id,
                output,
            }],
            Some((to, via)) => {
                if w.graph.step(&to).is_none() {
                    return vec![RunnerAction::FailRun {
                        run: self.id,
                        error: format!(
                            "step '{}' transitions to '{to}', which is not in this workflow",
                            last.step
                        ),
                    }];
                }
                vec![self.start(
                    w,
                    &to,
                    Some(index),
                    via,
                    &output_as_input(&output),
                    Some(&last.step.clone()),
                )]
            }
        }
    }
}

impl RunnerBehavior for WorkflowRunner {
    fn on_outcome(
        &self,
        state: &SessionState,
        agent: AgentId,
        end: TurnEnd,
        now_ms: u64,
    ) -> OutcomeDecision {
        // The agent must be one of this run's step executions; the dispatcher
        // found this runner by exactly that, but the guard keeps the decision
        // self-contained.
        let known = self
            .workflow(state)
            .is_some_and(|w| w.run.index_of_agent(agent.0).is_some());
        if !known {
            return OutcomeDecision::none();
        }
        let turn_ended = |end: RecordedEnd| SessionEvent::TurnEnded {
            at_ms: now_ms,
            agent,
            end,
        };
        match end {
            // The step's structured result — folded into its log entry, and
            // what the boundary reads to decide where the run goes next.
            TurnEnd::Concluded { output } => {
                OutcomeDecision::advance(vec![turn_ended(RecordedEnd::Concluded { output })])
            }
            // A parked step parks the run. No boundary: only an answer moves
            // it.
            TurnEnd::Asked => OutcomeDecision::record(vec![turn_ended(RecordedEnd::Asked)]),
            // Fails the step's entry and, through the fold, the run. A
            // boundary follows so a nested run's failure reaches the agent
            // that asked for it.
            TurnEnd::Failed { error, .. } => {
                OutcomeDecision::advance(vec![turn_ended(RecordedEnd::Failed { error })])
            }
            // Waiting on timers or subagents; something will wake it.
            TurnEnd::Parked => OutcomeDecision::none(),
            // Repaired from the run log at load, not from this report: the
            // step agent stays cold long enough that its own recovery runs
            // after the entry was reconciled.
            TurnEnd::Interrupted => OutcomeDecision::none(),
        }
    }

    /// The report this run owes whoever asked for it, then whatever the graph
    /// wants started.
    fn actions(&self, state: &SessionState) -> Vec<RunnerAction> {
        let Some(record) = state.record(self.id) else {
            return Vec::new();
        };
        let Some(w) = self.workflow(state) else {
            return Vec::new();
        };
        let mut actions: Vec<RunnerAction> = record
            .parent
            .and_then(|to| {
                deliver::run_part_for(self.id, w).map(|part| RunnerAction::Deliver {
                    to,
                    child: self.id,
                    part,
                })
            })
            .into_iter()
            .collect();
        actions.extend(self.step_actions(w));
        actions
    }

    fn repairs(&self, state: &SessionState) -> Vec<Repair> {
        let Some(w) = self.workflow(state) else {
            return Vec::new();
        };
        // A step the process died inside: suspend the run, which is the state
        // a retry can move.
        if w.run.current().is_some() {
            return vec![Repair::SuspendInterruptedRun { id: self.id }];
        }
        // A run that has not begun — created and never started, or loaded
        // before its first step. Let the boundary start it.
        if matches!(w.run.status, WorkflowRunStatus::Pending) {
            return vec![Repair::AdvanceRun { id: self.id }];
        }
        Vec::new()
    }

    fn busy(&self, state: &SessionState) -> bool {
        self.workflow(state)
            .is_some_and(|w| w.run.current().is_some())
    }

    /// Cancelling the step's agent is not enough: without this the entry stays
    /// `Running` for ever, so `current()` never clears and the graph starts
    /// nothing again — the run wedged while its page read "Running".
    /// `StepCancelled` suspends it, which is the state a retry can move.
    fn stop_event(
        &self,
        state: &SessionState,
        agent: AgentId,
        now_ms: u64,
    ) -> Option<SessionEvent> {
        let w = self.workflow(state)?;
        let index = w.run.index_of_agent(agent.0)?;
        (w.run.current() == Some(index)).then_some(SessionEvent::Runner {
            id: self.id,
            at_ms: now_ms,
            event: RunnerEvent::StepCancelled { index },
        })
    }

    fn role(&self, spec: &SessionSpec, state: &SessionState, agent: AgentId) -> Option<AgentRole> {
        let w = self.workflow(state)?;
        let index = w.run.index_of_agent(agent.0)?;
        let entry = w.run.get(index)?;
        let step = entry.step.clone();
        self.role_for_step(spec, state, &step, agent)
    }
}

impl WorkflowRunner {
    /// The role for one execution of `step_name`, resolvable *before* the
    /// execution is in the log: the spawner needs it while the `StepStarted`
    /// that records it is still in flight.
    pub(crate) fn role_for_step(
        &self,
        spec: &SessionSpec,
        state: &SessionState,
        step_name: &str,
        agent: AgentId,
    ) -> Option<AgentRole> {
        let w = self.workflow(state)?;
        let step = w.graph.step(step_name)?;
        Some(AgentRole {
            agent,
            name: agent.to_string(),
            journal: agent.0,
            // The step's own preset, snapshotted into the graph at the run's
            // creation — never the session's, which a workflow session does
            // not have.
            settings: step.settings.clone(),
            prompt_suffix: Some(STEP_PROMPT_SUFFIX),
            broadcasts: true,
            scoped: Some(agent.0),
            control_plane: false,
            // `ask_user` only when the definition says the step is
            // interactive, and somebody is there to answer.
            may_ask: step.interactive && !spec.is_unattended(),
            titles: TitleScope::None,
            step_result: Some(StepResultDef {
                outcomes: step.outcomes.clone(),
                fields: step.fields.clone(),
                interactive: step.interactive,
            }),
            stop_hook: StopHookKind::Stop,
            agent_type: None,
        })
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
    //! The run's decisions, ported from the orchestrator-era driver tests:
    //! same graph, same rules, now per-run.

    use super::super::testkit::*;
    use super::*;
    use crate::sessions::session_actor::runner::event::RecordedEnd;

    /// triage --p0--> fix --> review, triage --else--> file
    fn spec_graph() -> crate::sessions::workflow::WorkflowRunSpec {
        graph(
            "triage",
            vec![
                step("triage", vec![to("fix", &["p0"]), to("file", &[])]),
                step("fix", vec![to("review", &[])]),
                step("review", vec![]),
                step("file", vec![]),
            ],
        )
    }

    fn runner_and_state() -> (WorkflowRunner, SessionState) {
        let id = RunnerId(uuid::Uuid::new_v4());
        let state = fold(&[run_created(id, None, spec_graph())]);
        (WorkflowRunner { id }, state)
    }

    /// Perform whatever the runner asks against the state, so a test reads as
    /// the sequence of steps rather than as a fold.
    fn advance(r: &WorkflowRunner, state: &mut SessionState, output: Value) -> RunnerAction {
        let action = r.actions(state).remove(0);
        match &action {
            RunnerAction::StartStep { run, start } => {
                state.apply(&SessionEvent::Runner {
                    id: *run,
                    at_ms: 0,
                    event: RunnerEvent::StepStarted {
                        index: start.index,
                        step: start.step.clone(),
                        agent: start.agent,
                        attempt: start.attempt,
                        from: start.from,
                        via: start.via.clone(),
                        input: start.input.clone(),
                    },
                });
                state.apply(&SessionEvent::TurnEnded {
                    at_ms: 1,
                    agent: start.agent,
                    end: RecordedEnd::Concluded { output },
                });
            }
            RunnerAction::FinishRun { run, output } => state.apply(&SessionEvent::Runner {
                id: *run,
                at_ms: 2,
                event: RunnerEvent::RunFinished {
                    output: output.clone(),
                },
            }),
            RunnerAction::FailRun { run, error } => state.apply(&SessionEvent::Runner {
                id: *run,
                at_ms: 2,
                event: RunnerEvent::RunFailed {
                    error: error.clone(),
                },
            }),
            RunnerAction::Deliver { .. } => panic!("a rootless run never delivers"),
        }
        action
    }

    #[test]
    fn a_fresh_run_starts_at_the_start_step_with_the_run_input() {
        let (r, state) = runner_and_state();
        let actions = r.actions(&state);
        assert_eq!(actions.len(), 1);
        let RunnerAction::StartStep { run, start } = &actions[0] else {
            panic!("expected a step, got {:?}", actions[0]);
        };
        assert_eq!(*run, r.id);
        assert_eq!(start.index, 0);
        assert_eq!(start.step, "triage");
        assert_eq!(start.attempt, 1);
        assert_eq!(start.from, None);
        assert_eq!(start.agent, step_agent_id(r.id, 0));
        assert_eq!(start.input, "Do triage.\n\n## Input\nthe build is red");
    }

    #[test]
    fn a_matching_condition_picks_its_branch_and_records_which() {
        let (r, mut state) = runner_and_state();
        advance(&r, &mut state, serde_json::json!({"outcome": "p0"}));
        let action = r.actions(&state).remove(0);
        let RunnerAction::StartStep { start, .. } = &action else {
            panic!("expected a step, got {action:?}");
        };
        assert_eq!(start.step, "fix");
        assert_eq!(start.from, Some(0));
        assert_eq!(start.via.as_deref(), Some("outcome in [p0]"));
    }

    #[test]
    fn a_failing_condition_falls_through_to_the_catch_all() {
        let (r, mut state) = runner_and_state();
        advance(&r, &mut state, serde_json::json!({"outcome": "p2"}));
        let action = r.actions(&state).remove(0);
        let RunnerAction::StartStep { start, .. } = &action else {
            panic!("expected a step, got {action:?}");
        };
        assert_eq!(start.step, "file");
        assert!(start.via.is_none(), "the catch-all records no condition");
    }

    #[test]
    fn a_step_with_no_transitions_finishes_the_run_with_its_output() {
        let (r, mut state) = runner_and_state();
        advance(&r, &mut state, serde_json::json!({"outcome": "p2"}));
        advance(&r, &mut state, serde_json::json!({"filed": 12}));
        let action = advance(&r, &mut state, serde_json::Value::Null);
        let RunnerAction::FinishRun { output, .. } = &action else {
            panic!("expected the run to finish, got {action:?}");
        };
        assert_eq!(output, &serde_json::json!({"filed": 12}));
        assert!(
            r.actions(&state).is_empty(),
            "a finished run starts nothing"
        );
    }

    /// Nothing else bounds a graph with a loop.
    #[test]
    fn a_loop_is_stopped_by_the_step_budget() {
        let id = RunnerId(uuid::Uuid::new_v4());
        let mut g = graph("a", vec![step("a", vec![to("a", &[])])]);
        g.max_steps = 3;
        let mut state = fold(&[run_created(id, None, g)]);
        let r = WorkflowRunner { id };
        for _ in 0..3 {
            advance(&r, &mut state, serde_json::json!({}));
        }
        let action = advance(&r, &mut state, serde_json::json!({}));
        let RunnerAction::FailRun { error, .. } = &action else {
            panic!("expected the budget to stop it, got {action:?}");
        };
        assert!(error.contains("step budget exhausted after 3"), "{error}");
    }

    #[test]
    fn nothing_starts_while_a_step_is_in_flight_or_after_it_failed() {
        let (r, mut state) = runner_and_state();
        let agent = step_agent_id(r.id, 0);
        state.apply(&step_started(r.id, 0, "triage", agent, 200));
        assert!(r.actions(&state).is_empty());
        state.apply(&ended(
            agent,
            RecordedEnd::Failed {
                error: "boom".into(),
            },
            300,
        ));
        assert!(r.actions(&state).is_empty());
    }

    #[test]
    fn a_suspended_or_parked_run_starts_nothing() {
        let (r, mut state) = runner_and_state();
        let agent = step_agent_id(r.id, 0);
        state.apply(&step_started(r.id, 0, "triage", agent, 200));
        state.apply(&ended(agent, RecordedEnd::Asked, 300));
        assert!(r.actions(&state).is_empty(), "parked run starts nothing");
        let mut state = fold(&[
            run_created(r.id, None, spec_graph()),
            step_started(r.id, 0, "triage", agent, 200),
        ]);
        state.apply(&SessionEvent::Runner {
            id: r.id,
            at_ms: 300,
            event: RunnerEvent::StepCancelled { index: 0 },
        });
        assert!(r.actions(&state).is_empty(), "suspended run starts nothing");
    }

    #[test]
    fn a_transition_to_a_missing_step_fails_the_run() {
        let id = RunnerId(uuid::Uuid::new_v4());
        let g = graph("a", vec![step("a", vec![to("ghost", &[])])]);
        let mut state = fold(&[run_created(id, None, g)]);
        let r = WorkflowRunner { id };
        advance(&r, &mut state, serde_json::json!({}));
        let action = r.actions(&state).remove(0);
        let RunnerAction::FailRun { error, .. } = &action else {
            panic!("expected a failure, got {action:?}");
        };
        assert!(error.contains("'ghost'"), "{error}");
    }

    // -- outcomes ----------------------------------------------------------

    #[test]
    fn a_steps_conclusion_advances_and_its_ask_parks() {
        let (r, mut state) = runner_and_state();
        let agent = step_agent_id(r.id, 0);
        state.apply(&step_started(r.id, 0, "triage", agent, 200));
        let d = r.on_outcome(
            &state,
            agent,
            TurnEnd::Concluded {
                output: serde_json::json!({"outcome": "p0"}),
            },
            300,
        );
        assert!(d.advance);
        assert_eq!(d.events.len(), 1);
        let d = r.on_outcome(&state, agent, TurnEnd::Asked, 300);
        assert!(!d.advance, "an ask is a park, not a boundary");
        let d = r.on_outcome(&state, agent, TurnEnd::Parked, 300);
        assert!(d.events.is_empty(), "a park journals nothing");
        let d = r.on_outcome(&state, agent, TurnEnd::Interrupted, 300);
        assert!(d.events.is_empty(), "an interruption is repaired at load");
    }

    #[test]
    fn an_outcome_from_an_agent_not_in_the_log_is_ignored() {
        let (r, state) = runner_and_state();
        let d = r.on_outcome(
            &state,
            agent(),
            TurnEnd::Concluded {
                output: serde_json::Value::Null,
            },
            300,
        );
        assert!(d.events.is_empty());
    }

    // -- stop, repair, role ------------------------------------------------

    #[test]
    fn stopping_the_current_step_cancels_its_entry() {
        let (r, mut state) = runner_and_state();
        let agent = step_agent_id(r.id, 0);
        state.apply(&step_started(r.id, 0, "triage", agent, 200));
        let event = r.stop_event(&state, agent, 300).unwrap();
        let SessionEvent::Runner {
            event: RunnerEvent::StepCancelled { index },
            ..
        } = event
        else {
            panic!("expected a cancel");
        };
        assert_eq!(index, 0);
        // A concluded step has nothing to stop.
        state.apply(&ended(
            agent,
            RecordedEnd::Concluded {
                output: serde_json::Value::Null,
            },
            400,
        ));
        assert!(r.stop_event(&state, agent, 500).is_none());
    }

    #[test]
    fn an_interrupted_step_suspends_and_a_pending_run_advances() {
        let (r, mut state) = runner_and_state();
        assert_eq!(r.repairs(&state), vec![Repair::AdvanceRun { id: r.id }]);
        let agent = step_agent_id(r.id, 0);
        state.apply(&step_started(r.id, 0, "triage", agent, 200));
        assert_eq!(
            r.repairs(&state),
            vec![Repair::SuspendInterruptedRun { id: r.id }]
        );
        assert!(r.busy(&state));
    }

    #[test]
    fn a_steps_role_speaks_its_own_preset_and_contract() {
        let (r, mut state) = runner_and_state();
        let agent = step_agent_id(r.id, 0);
        state.apply(&step_started(r.id, 0, "triage", agent, 200));
        let spec = crate::sessions::session_actor::testing::actor_spec_fixture();
        let role = r.role(&spec, &state, agent).unwrap();
        assert_eq!(role.journal, agent.0);
        assert_eq!(role.settings.model, "mock");
        assert!(role.requires_result());
        assert!(!role.may_ask, "triage is not interactive");
        assert_eq!(role.titles, TitleScope::None);
        assert!(role.broadcasts);
        assert_eq!(role.scoped, Some(agent.0));
    }
}
