//! Every result a child runner owes an agent that the agent has not been
//! sent.
//!
//! One rule covers the tree: **a runner with a parent, a terminal result, and
//! an unsent flag is owed**. A subagent's report and a nested workflow run's
//! output are the same shape to the agent waiting on them, which is why
//! nesting workflow runs added no delivery machinery — only a second arm in
//! [`owed_part`].
//!
//! Unconditional: whether the recipient is idle, running or parked is not a
//! question this answers, because the result goes into a queue rather than
//! into a turn. The agent decides when its queue becomes a turn, and a report
//! deliberately waits out a park rather than overriding it.

use horsie_models::agent::SubAgentResultPart;

use super::action::RunnerAction;
use super::ids::RunnerId;
use super::state::{RunnerState, SessionState, SubPhase, SubState, WorkflowState};

/// Largest result (output or error) injected into a parent's context or
/// rendered by `subagent_status` — the same bound the runtime puts on a
/// tool's streamed output.
pub const MAX_RESULT_BYTES: usize = 50_000;

/// Cap a result for injection/rendering, marking the cut so the reader knows
/// the answer continues elsewhere (the full transcript is always in the
/// child's own history).
pub(crate) fn truncate_result(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_RESULT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…\n\n[truncated: {} bytes total]",
        &text[..end],
        text.len()
    )
}

/// Every delivery the session owes, across the whole tree.
#[must_use]
pub(crate) fn owed_deliveries(state: &SessionState) -> Vec<RunnerAction> {
    state
        .runners
        .iter()
        .filter_map(|(id, record)| {
            let to = record.parent?;
            let part = owed_part(*id, &record.state)?;
            Some(RunnerAction::Deliver {
                to,
                child: *id,
                part,
            })
        })
        .collect()
}

/// The unsent terminal result of one runner, if it holds one.
fn owed_part(id: RunnerId, state: &RunnerState) -> Option<SubAgentResultPart> {
    match state {
        RunnerState::Sub(sub) => sub_part_for(id, sub),
        RunnerState::Workflow(w) => run_part_for(id, w),
        // Conversations owe nobody a result — that is what makes them
        // conversations.
        RunnerState::Main(_) | RunnerState::Fork(_) => None,
    }
}

/// A subagent's report, as a structured part. The result decides the body: a
/// node that concluded once and failed on a later cycle reports the failure,
/// never the stale success.
pub(crate) fn sub_part_for(id: RunnerId, sub: &SubState) -> Option<SubAgentResultPart> {
    let SubPhase::Done {
        result,
        started_ms,
        ended_ms,
        notified: false,
    } = &sub.phase
    else {
        return None;
    };
    let (status, body) = match result {
        Ok(output) => ("completed", output.as_str()),
        Err(error) => ("failed", error.as_str()),
    };
    Some(SubAgentResultPart {
        subagent_id: id.to_string(),
        label: sub.label.clone(),
        status: status.to_string(),
        text: truncate_result(body),
        spawned_at_ms: *started_ms,
        ended_at_ms: *ended_ms,
    })
}

/// A finished run's output, in the same part a subagent's report rides —
/// labeled with the workflow's name, so the caller reads `[subagent
/// "fix-bug" completed]` for the run it asked for.
pub(crate) fn run_part_for(id: RunnerId, w: &WorkflowState) -> Option<SubAgentResultPart> {
    use crate::sessions::workflow::WorkflowRunStatus;
    if w.notified {
        return None;
    }
    let (status, body) = match w.run.status {
        WorkflowRunStatus::Finished => (
            "completed",
            w.run
                .output
                .as_ref()
                .map(crate::sessions::workflow::render_result)
                .unwrap_or_default(),
        ),
        WorkflowRunStatus::Failed => (
            "failed",
            w.run.error.clone().unwrap_or_else(|| "failed".to_string()),
        ),
        WorkflowRunStatus::Pending
        | WorkflowRunStatus::Running
        | WorkflowRunStatus::Suspended
        | WorkflowRunStatus::AwaitingInput => return None,
    };
    let started = w.run.steps.first().map(|s| s.started_at_ms).unwrap_or(0);
    let ended = w
        .run
        .steps
        .iter()
        .filter_map(|s| s.ended_at_ms)
        .max()
        .unwrap_or(0);
    Some(SubAgentResultPart {
        subagent_id: id.to_string(),
        label: w.graph.workflow.clone(),
        status: status.to_string(),
        text: truncate_result(&body),
        spawned_at_ms: started,
        ended_at_ms: ended,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::super::event::{RecordedEnd, RunnerEvent, SessionEvent};
    use super::super::ids::RunnerId;
    use super::super::testkit::*;
    use super::*;

    #[test]
    fn nothing_is_owed_in_a_fresh_session() {
        let main = agent();
        let state = fold(&[main_created(main)]);
        assert!(owed_deliveries(&state).is_empty());
    }

    #[test]
    fn a_finished_child_is_owed_to_the_agent_that_asked() {
        let main = agent();
        let sub = agent();
        let state = fold(&[
            main_created(main),
            sub_created(sub, main, 100),
            ended(
                sub,
                RecordedEnd::Concluded {
                    output: "three stale crates".into(),
                },
                400,
            ),
        ]);
        let actions = owed_deliveries(&state);
        assert_eq!(actions.len(), 1);
        let RunnerAction::Deliver { to, child, part } = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(*to, main);
        assert_eq!(*child, RunnerId::of_agent(sub));
        assert_eq!(part.text, "three stale crates");
        assert_eq!(part.label, "worker");
        assert_eq!(part.status, "completed");
        assert_eq!((part.spawned_at_ms, part.ended_at_ms), (100, 400));
    }

    #[test]
    fn a_result_already_sent_is_not_owed_again() {
        let main = agent();
        let sub = agent();
        let mut state = fold(&[
            main_created(main),
            sub_created(sub, main, 100),
            ended(
                sub,
                RecordedEnd::Concluded {
                    output: "done".into(),
                },
                400,
            ),
        ]);
        state.apply(&SessionEvent::Runner {
            id: RunnerId::of_agent(sub),
            at_ms: 401,
            event: RunnerEvent::Reported,
        });
        assert!(owed_deliveries(&state).is_empty());
    }

    #[test]
    fn a_nested_child_is_owed_to_the_subagent_that_spawned_it() {
        let main = agent();
        let lead = agent();
        let helper = agent();
        let mut state = fold(&[
            main_created(main),
            sub_created(lead, main, 100),
            ended(
                lead,
                RecordedEnd::Concluded {
                    output: "waiting".into(),
                },
                200,
            ),
        ]);
        state.apply(&SessionEvent::Runner {
            id: RunnerId::of_agent(lead),
            at_ms: 201,
            event: RunnerEvent::Reported,
        });
        state.apply(&sub_created(helper, lead, 300));
        state.apply(&ended(
            helper,
            RecordedEnd::Concluded {
                output: "kid done".into(),
            },
            600,
        ));
        let actions = owed_deliveries(&state);
        assert_eq!(actions.len(), 1);
        let RunnerAction::Deliver { to, part, .. } = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(*to, lead);
        assert_eq!(part.text, "kid done");
    }

    /// A node that concluded once and failed on a later cycle reports the
    /// failure — status decides the body, so a stale success cannot mask it.
    #[test]
    fn a_later_failure_reports_the_failure_not_the_earlier_output() {
        let main = agent();
        let sub = agent();
        let mut state = fold(&[
            main_created(main),
            sub_created(sub, main, 100),
            ended(
                sub,
                RecordedEnd::Concluded {
                    output: "first pass".into(),
                },
                400,
            ),
        ]);
        state.apply(&SessionEvent::Runner {
            id: RunnerId::of_agent(sub),
            at_ms: 401,
            event: RunnerEvent::Reported,
        });
        state.apply(&SessionEvent::TurnBegan {
            at_ms: 500,
            agent: sub,
        });
        state.apply(&ended(
            sub,
            RecordedEnd::Failed {
                error: "second pass blew up".into(),
            },
            900,
        ));
        let actions = owed_deliveries(&state);
        let RunnerAction::Deliver { part, .. } = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "second pass blew up");
    }

    #[test]
    fn a_finished_nested_run_is_owed_as_a_completed_part_named_after_its_workflow() {
        let main = agent();
        let run = RunnerId(uuid::Uuid::new_v4());
        let step_agent = super::super::step_agent_id(run, 0);
        let g = graph("triage", vec![step("triage", vec![])]);
        let state = fold(&[
            main_created(main),
            run_created(run, Some(main), g),
            step_started(run, 0, "triage", step_agent, 200),
            ended(
                step_agent,
                RecordedEnd::Concluded {
                    output: serde_json::json!({"outcome": "success", "description": "all good"}),
                },
                300,
            ),
            SessionEvent::Runner {
                id: run,
                at_ms: 400,
                event: RunnerEvent::RunFinished {
                    output: serde_json::json!({"outcome": "success", "description": "all good"}),
                },
            },
        ]);
        let actions = owed_deliveries(&state);
        assert_eq!(actions.len(), 1);
        let RunnerAction::Deliver { to, child, part } = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(*to, main);
        assert_eq!(*child, run);
        assert_eq!(part.status, "completed");
        assert_eq!(part.label, "fix-bug");
        assert!(part.text.contains("all good"), "{}", part.text);
        assert_eq!((part.spawned_at_ms, part.ended_at_ms), (200, 300));
    }

    #[test]
    fn a_failed_nested_run_is_owed_as_a_failed_part() {
        let main = agent();
        let run = RunnerId(uuid::Uuid::new_v4());
        let g = graph("triage", vec![step("triage", vec![])]);
        let state = fold(&[
            main_created(main),
            run_created(run, Some(main), g),
            SessionEvent::Runner {
                id: run,
                at_ms: 400,
                event: RunnerEvent::RunFailed {
                    error: "start step 'x' is not in this workflow".into(),
                },
            },
        ]);
        let actions = owed_deliveries(&state);
        let RunnerAction::Deliver { part, .. } = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(part.status, "failed");
        assert!(part.text.contains("not in this workflow"));
    }

    /// The session-root run has no parent: nobody asked, so nothing is owed.
    #[test]
    fn a_root_run_owes_nobody() {
        let run = RunnerId(uuid::Uuid::new_v4());
        let g = graph("triage", vec![step("triage", vec![])]);
        let state = fold(&[
            run_created(run, None, g),
            SessionEvent::Runner {
                id: run,
                at_ms: 400,
                event: RunnerEvent::RunFinished {
                    output: serde_json::Value::Null,
                },
            },
        ]);
        assert!(owed_deliveries(&state).is_empty());
    }

    #[test]
    fn an_owed_part_caps_a_huge_result() {
        let main = agent();
        let sub = agent();
        let huge = "x".repeat(MAX_RESULT_BYTES + 10_000);
        let state = fold(&[
            main_created(main),
            sub_created(sub, main, 100),
            ended(
                sub,
                RecordedEnd::Concluded {
                    output: huge.clone().into(),
                },
                400,
            ),
        ]);
        let RunnerAction::Deliver { part, .. } = &owed_deliveries(&state)[0] else {
            panic!("expected a delivery");
        };
        let text = part.to_wire_text();
        assert!(text.contains("[truncated:"), "{:.200}", text);
        assert!(text.len() < huge.len());
        assert!(text.len() <= MAX_RESULT_BYTES + 100);
    }
}
