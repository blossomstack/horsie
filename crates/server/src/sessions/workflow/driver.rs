//! The one piece of the old workflow driver that survived the runner swap.
//!
//! `WorkflowOrchestrator` is gone: deciding what a run does next is
//! [`crate::sessions::runners::workflow::State`]'s, where the run's own slice
//! is. What is left is the pure transition lookup both shapes needed, which
//! never belonged to the orchestrator in the first place.

pub fn next_transition(
    transitions: &[crate::sessions::workflow::TransitionSpec],
    outcome: &str,
) -> Option<(String, Option<String>)> {
    for t in transitions {
        let Some(filter) = &t.when else {
            return Some((t.to.clone(), None));
        };
        if filter.matches(outcome) {
            return Some((t.to.clone(), Some(filter.render())));
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::spec::AgentSettings;
    use crate::sessions::workflow::WorkflowRunStatus;
    use crate::sessions::workflow::spec::{TransitionSpec, WorkflowStepSpec};
    use horsie_models::workflow::OutcomeFilter;

    fn settings() -> AgentSettings {
        AgentSettings {
            instructions: None,
            model: "sonnet".into(),
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: Vec::new(),
            memory_spaces: Vec::new(),
            thinking_effort: None,
            max_concurrent_subagents: None,
            auto_compact: None,
            control_plane: None,
            plugins: Vec::new(),
        }
    }

    fn step(name: &str, transitions: Vec<TransitionSpec>) -> WorkflowStepSpec {
        WorkflowStepSpec {
            name: name.into(),
            agent: "a".into(),
            prompt: format!("Do {name}."),
            outcomes: crate::sessions::workflow::default_outcomes(),
            fields: Vec::new(),
            interactive: false,
            transitions,
            settings: settings(),
        }
    }

    /// A transition taken for any of `values`, or a catch-all when empty.
    fn to(target: &str, values: &[&str]) -> TransitionSpec {
        TransitionSpec {
            to: target.into(),
            when: (!values.is_empty()).then(|| {
                OutcomeFilter::In(horsie_models::workflow::OutcomeIn {
                    values: values.iter().map(|v| (*v).to_string()).collect(),
                })
            }),
        }
    }

    fn not_in(target: &str, values: &[&str]) -> TransitionSpec {
        TransitionSpec {
            to: target.into(),
            when: Some(OutcomeFilter::NotIn(
                horsie_models::workflow::OutcomeNotIn {
                    values: values.iter().map(|v| (*v).to_string()).collect(),
                },
            )),
        }
    }

    /// triage --p0--> fix --> review, triage --else--> file
    fn spec() -> Arc<WorkflowRunSpec> {
        Arc::new(WorkflowRunSpec {
            workflow: "fix-bug".into(),
            start: "triage".into(),
            steps: vec![
                step("triage", vec![to("fix", &["p0"]), to("file", &[])]),
                step("fix", vec![to("review", &[])]),
                step("review", vec![]),
                step("file", vec![]),
            ],
            input: "the build is red".into(),
            max_steps: 100,
        })
    }

    fn driver() -> (WorkflowOrchestrator, Uuid) {
        let id = Uuid::new_v4();
        (WorkflowOrchestrator::new(id, spec()), id)
    }

    fn state(run: WorkflowRunState) -> SessionState {
        SessionState {
            run: Some(run),
            ..SessionState::default()
        }
    }

    /// Drive the run forward by performing whatever the driver asks, so a test
    /// reads as the sequence of steps rather than as a fold.
    fn advance(d: &WorkflowOrchestrator, run: &mut WorkflowRunState, output: Value) -> AgentAction {
        let action = d.step_actions(&state(run.clone())).remove(0);
        match &action {
            AgentAction::StartStep(StepStart {
                step,
                agent,
                attempt,
                from,
                via,
                input,
                ..
            }) => {
                run.apply_started(
                    step.clone(),
                    *agent,
                    *attempt,
                    *from,
                    via.clone(),
                    input.clone(),
                    0,
                );
                let index = (run.steps.len() - 1) as u32;
                run.apply_concluded(index, output, 1);
            }
            AgentAction::Finish { output } => run.apply_finished(output.clone()),
            AgentAction::Fail { error } => run.apply_failed(error.clone()),
            AgentAction::Deliver(_) => panic!("a run's step decisions never deliver a result"),
        }
        action
    }

    #[test]
    fn a_fresh_run_starts_at_the_start_step_with_the_run_input() {
        let (d, session) = driver();
        let actions = d.step_actions(&state(WorkflowRunState::default()));
        assert_eq!(actions.len(), 1);
        let AgentAction::StartStep(StepStart {
            index,
            step,
            agent,
            attempt,
            from,
            input,
            ..
        }) = &actions[0]
        else {
            panic!("expected a step, got {:?}", actions[0]);
        };
        assert_eq!(*index, 0);
        assert_eq!(step, "triage");
        assert_eq!(*attempt, 1);
        assert_eq!(*from, None);
        assert_eq!(*agent, WorkflowRunSpec::step_agent_id(session, 0));
        assert_eq!(input, "Do triage.\n\n## Input\nthe build is red");
    }

    #[test]
    fn a_matching_condition_picks_its_branch_and_records_which() {
        let (d, _) = driver();
        let mut run = WorkflowRunState::default();
        advance(&d, &mut run, serde_json::json!({"outcome": "p0"}));
        let action = advance(&d, &mut run, serde_json::json!({}));
        let AgentAction::StartStep(StepStart {
            step, via, from, ..
        }) = &action
        else {
            panic!("expected a step, got {action:?}");
        };
        assert_eq!(step, "fix");
        assert_eq!(*from, Some(0));
        assert_eq!(via.as_deref(), Some("outcome in [p0]"));
    }

    #[test]
    fn a_failing_condition_falls_through_to_the_catch_all() {
        let (d, _) = driver();
        let mut run = WorkflowRunState::default();
        advance(&d, &mut run, serde_json::json!({"outcome": "p2"}));
        let action = advance(&d, &mut run, serde_json::json!({}));
        let AgentAction::StartStep(StepStart { step, via, .. }) = &action else {
            panic!("expected a step, got {action:?}");
        };
        assert_eq!(step, "file");
        assert!(via.is_none(), "the catch-all records no condition");
    }

    #[test]
    fn a_step_with_no_transitions_finishes_the_run_with_its_output() {
        let (d, _) = driver();
        let mut run = WorkflowRunState::default();
        advance(&d, &mut run, serde_json::json!({"outcome": "p2"}));
        advance(&d, &mut run, serde_json::json!({"filed": 12}));
        let action = advance(&d, &mut run, serde_json::json!({}));
        let AgentAction::Finish { output } = &action else {
            panic!("expected the run to finish, got {action:?}");
        };
        assert_eq!(output, &serde_json::json!({"filed": 12}));
        assert_eq!(run.status, WorkflowRunStatus::Finished);
    }

    /// Nothing else bounds a graph with a loop.
    #[test]
    fn a_loop_is_stopped_by_the_step_budget() {
        let d = WorkflowOrchestrator::new(
            Uuid::new_v4(),
            Arc::new(WorkflowRunSpec {
                workflow: "w".into(),
                start: "a".into(),
                steps: vec![step("a", vec![to("a", &[])])],
                input: "x".into(),
                max_steps: 3,
            }),
        );
        let mut run = WorkflowRunState::default();
        for _ in 0..3 {
            advance(&d, &mut run, serde_json::json!({}));
        }
        let action = advance(&d, &mut run, serde_json::json!({}));
        let AgentAction::Fail { error } = &action else {
            panic!("expected the budget to stop it, got {action:?}");
        };
        assert!(error.contains("step budget exhausted after 3"), "{error}");
        assert_eq!(run.steps.len(), 3, "the budget is checked before starting");
    }

    #[test]
    fn nothing_starts_while_a_step_is_in_flight() {
        let (d, session) = driver();
        let mut run = WorkflowRunState::default();
        run.apply_started(
            "triage".into(),
            WorkflowRunSpec::step_agent_id(session, 0),
            1,
            None,
            None,
            "in".into(),
            0,
        );
        assert!(d.step_actions(&state(run)).is_empty());
    }

    /// A suspended run waits for a person: an interrupted step's effect on the
    /// shared workspace is unknown, so it is not resumed by itself.
    #[test]
    fn a_suspended_or_parked_run_starts_nothing() {
        let (d, _) = driver();
        for status in [
            WorkflowRunStatus::Suspended,
            WorkflowRunStatus::AwaitingInput,
            WorkflowRunStatus::Finished,
            WorkflowRunStatus::Failed,
        ] {
            let run = WorkflowRunState {
                status,
                ..WorkflowRunState::default()
            };
            assert!(
                d.step_actions(&state(run)).is_empty(),
                "{status:?} must start nothing"
            );
        }
    }

    #[test]
    fn a_failed_step_starts_nothing_more() {
        let (d, session) = driver();
        let mut run = WorkflowRunState::default();
        run.apply_started(
            "triage".into(),
            WorkflowRunSpec::step_agent_id(session, 0),
            1,
            None,
            None,
            "in".into(),
            0,
        );
        run.apply_step_failed(0, "provider 500".into(), 1);
        assert!(d.step_actions(&state(run)).is_empty());
    }

    /// The definition is snapshotted, so this can only happen to a run whose
    /// snapshot is itself inconsistent — but it must not panic or hang.
    #[test]
    fn a_transition_to_a_missing_step_fails_the_run() {
        let d = WorkflowOrchestrator::new(
            Uuid::new_v4(),
            Arc::new(WorkflowRunSpec {
                workflow: "w".into(),
                start: "a".into(),
                steps: vec![step("a", vec![to("ghost", &[])])],
                input: "x".into(),
                max_steps: 100,
            }),
        );
        let mut run = WorkflowRunState::default();
        advance(&d, &mut run, serde_json::json!({}));
        let action = advance(&d, &mut run, serde_json::json!({}));
        let AgentAction::Fail { error } = &action else {
            panic!("expected a failure, got {action:?}");
        };
        assert!(error.contains("'ghost'"), "{error}");
    }

    #[test]
    fn not_in_matches_everything_it_does_not_name() {
        let d = WorkflowOrchestrator::new(
            Uuid::new_v4(),
            Arc::new(WorkflowRunSpec {
                workflow: "w".into(),
                start: "a".into(),
                steps: vec![
                    step("a", vec![not_in("b", &["p0"]), to("c", &[])]),
                    step("b", vec![]),
                    step("c", vec![]),
                ],
                input: "x".into(),
                max_steps: 100,
            }),
        );
        let mut run = WorkflowRunState::default();
        advance(&d, &mut run, serde_json::json!({"outcome": "p2"}));
        let action = advance(&d, &mut run, serde_json::json!({"outcome": "p2"}));
        let AgentAction::StartStep(StepStart { step, via, .. }) = &action else {
            panic!("expected a step, got {action:?}");
        };
        assert_eq!(step, "b", "p2 is not p0, so the negative filter admits it");
        assert_eq!(via.as_deref(), Some("outcome not in [p0]"));
    }

    #[test]
    fn a_not_in_filter_that_names_the_outcome_falls_through() {
        let d = WorkflowOrchestrator::new(
            Uuid::new_v4(),
            Arc::new(WorkflowRunSpec {
                workflow: "w".into(),
                start: "a".into(),
                steps: vec![
                    step("a", vec![not_in("b", &["p0"]), to("c", &[])]),
                    step("b", vec![]),
                    step("c", vec![]),
                ],
                input: "x".into(),
                max_steps: 100,
            }),
        );
        let mut run = WorkflowRunState::default();
        advance(&d, &mut run, serde_json::json!({"outcome": "p0"}));
        let action = advance(&d, &mut run, serde_json::json!({"outcome": "p0"}));
        let AgentAction::StartStep(StepStart { step, .. }) = &action else {
            panic!("expected a step, got {action:?}");
        };
        assert_eq!(step, "c");
    }

    /// An outcome the step never declared cannot reach here — `submit_result`
    /// rejects it — but if one did, it must match nothing rather than match
    /// everything.
    #[test]
    fn an_unrecognised_outcome_matches_no_filter() {
        let filter = OutcomeFilter::In(horsie_models::workflow::OutcomeIn {
            values: vec!["p0".into()],
        });
        assert!(!filter.matches("p9"));
    }

    #[test]
    fn a_filter_renders_as_the_edge_label_a_reader_sees() {
        let f = OutcomeFilter::In(horsie_models::workflow::OutcomeIn {
            values: vec!["p0".into(), "p1".into()],
        });
        assert_eq!(f.render(), "outcome in [p0, p1]");
        let f = OutcomeFilter::NotIn(horsie_models::workflow::OutcomeNotIn {
            values: vec!["p2".into()],
        });
        assert_eq!(f.render(), "outcome not in [p2]");
    }

    /// The driver is installed only on a run, so a state that has folded no
    /// step yet means the run is about to begin — not that it is a
    /// conversation.
    #[test]
    fn a_state_with_no_run_folded_yet_starts_the_first_step() {
        let (d, _) = driver();
        let actions = d.step_actions(&SessionState::default());
        assert_eq!(actions.len(), 1);
        let AgentAction::StartStep(StepStart { step, index, .. }) = &actions[0] else {
            panic!("expected the start step, got {:?}", actions[0]);
        };
        assert_eq!(step, "triage");
        assert_eq!(*index, 0);
    }
}
