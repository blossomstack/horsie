//! Fold a job's workflow journal into a [`JobProgress`] for `horsie job status`.
//!
//! Pure and time-free: the rows carry only timestamps (the source of truth), and
//! the CLI derives durations from them. Rows are the actual execution trace — an
//! agent revisited in a loop appears more than once — followed by any workflow
//! definition agents that were never visited, as `Pending`.

use models::daemon::{AgentPhase, AgentProgress, JobProgress, JobStatus};
use models::workflow::WorkflowDefinition;
use workflow::WorkflowDomainEvent;

/// Close the open (last) trace row at `at`, marking it `Done`.
fn close_last(rows: &mut [AgentProgress], at: Option<u64>) {
    if let Some(last) = rows.last_mut() {
        last.ended_at = at;
        last.phase = AgentPhase::Done;
    }
}

/// Build a [`JobProgress`] from a job's ordered workflow events, the workflow
/// definition (for the pending tail), the overall job status, and the submit
/// time. `job_id`/`workflow_name` are left empty for the caller to fill.
pub fn fold_progress(
    events: &[WorkflowDomainEvent],
    def: &WorkflowDefinition,
    status: JobStatus,
    submitted_at: u64,
) -> JobProgress {
    let mut rows: Vec<AgentProgress> = Vec::new();
    let mut finished_at: Option<u64> = None;

    for ev in events {
        match ev {
            WorkflowDomainEvent::AgentStarted {
                agent_name, at_ms, ..
            } => {
                close_last(&mut rows, *at_ms);
                rows.push(AgentProgress {
                    name: agent_name.clone(),
                    phase: AgentPhase::Active,
                    started_at: *at_ms,
                    ended_at: None,
                });
            }
            WorkflowDomainEvent::AgentTransitioned { at_ms, .. } => {
                close_last(&mut rows, *at_ms);
            }
            WorkflowDomainEvent::WorkflowFinished { at_ms, .. }
            | WorkflowDomainEvent::WorkflowFailed { at_ms, .. } => {
                close_last(&mut rows, *at_ms);
                finished_at = *at_ms;
            }
            // Pause/Park/Suspend/Resume/Start do not close a row: the current
            // agent stays Active; the overall `status` qualifies its label.
            WorkflowDomainEvent::WorkflowStarted { .. }
            | WorkflowDomainEvent::WorkflowSuspended { .. }
            | WorkflowDomainEvent::WorkflowPaused { .. }
            | WorkflowDomainEvent::WorkflowResumed { .. }
            | WorkflowDomainEvent::WorkflowParked { .. } => {}
        }
    }

    // A terminal job's last row is Done (closed above when the terminal event
    // carried a timestamp; ensure the phase regardless). A non-terminal job's
    // last row stays Active — its `ended_at` is None, so the CLI uses "now".
    if matches!(status, JobStatus::Finished | JobStatus::Failed)
        && let Some(last) = rows.last_mut()
    {
        last.phase = AgentPhase::Done;
    }

    // Append never-visited definition agents as Pending.
    for a in &def.agents {
        if !rows.iter().any(|r| r.name == a.name) {
            rows.push(AgentProgress {
                name: a.name.clone(),
                phase: AgentPhase::Pending,
                started_at: None,
                ended_at: None,
            });
        }
    }

    JobProgress {
        job_id: String::new(),
        workflow_name: String::new(),
        status,
        submitted_at,
        finished_at,
        agents: rows,
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
    use models::workflow::WorkflowAgentDef;
    use serde_json::Value;
    use uuid::Uuid;

    fn def(names: &[&str]) -> WorkflowDefinition {
        WorkflowDefinition {
            default_use_plugins: None,
            start: names.first().copied().unwrap_or("").into(),
            agents: names
                .iter()
                .map(|n| WorkflowAgentDef {
                    use_plugins: None,
                    name: (*n).into(),
                    system_prompt: None,
                    model: "mock".into(),
                    output_schema: None,
                    allow_ask_user: false,
                    allow_timers: None,
                    transitions: None,
                    max_iterations: None,
                    max_retries: None,
                    allowed_tools: None,
                })
                .collect(),
        }
    }

    fn started(name: &str, at: Option<u64>) -> WorkflowDomainEvent {
        WorkflowDomainEvent::AgentStarted {
            agent_name: name.into(),
            session_id: Uuid::new_v4(),
            input: String::new(),
            at_ms: at,
        }
    }

    fn transitioned(from: &str, to: &str, at: Option<u64>) -> WorkflowDomainEvent {
        WorkflowDomainEvent::AgentTransitioned {
            from: from.into(),
            to: to.into(),
            from_session: Uuid::new_v4(),
            to_session: Uuid::new_v4(),
            condition: None,
            at_ms: at,
        }
    }

    fn finished(at: Option<u64>) -> WorkflowDomainEvent {
        WorkflowDomainEvent::WorkflowFinished {
            output: Value::Null,
            at_ms: at,
        }
    }

    #[test]
    fn linear_path_with_timestamps() {
        let events = vec![
            WorkflowDomainEvent::WorkflowStarted { at_ms: Some(1000) },
            started("planner", Some(1000)),
            transitioned("planner", "coder", Some(2000)),
            started("coder", Some(2000)),
        ];
        let p = fold_progress(
            &events,
            &def(&["planner", "coder"]),
            JobStatus::Running,
            500,
        );
        assert_eq!(p.agents.len(), 2);
        assert_eq!(p.agents[0].name, "planner");
        assert!(matches!(p.agents[0].phase, AgentPhase::Done));
        assert_eq!(p.agents[0].started_at, Some(1000));
        assert_eq!(p.agents[0].ended_at, Some(2000));
        assert_eq!(p.agents[1].name, "coder");
        assert!(matches!(p.agents[1].phase, AgentPhase::Active));
        assert_eq!(p.agents[1].started_at, Some(2000));
        assert_eq!(p.agents[1].ended_at, None);
        assert_eq!(p.finished_at, None);
    }

    #[test]
    fn loop_yields_a_row_per_visit() {
        let events = vec![
            started("planner", Some(1)),
            transitioned("planner", "coder", Some(2)),
            started("coder", Some(2)),
            transitioned("coder", "planner", Some(3)),
            started("planner", Some(3)),
        ];
        let p = fold_progress(&events, &def(&["planner", "coder"]), JobStatus::Running, 0);
        let names: Vec<_> = p.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["planner", "coder", "planner"]);
        assert!(matches!(p.agents[2].phase, AgentPhase::Active));
    }

    #[test]
    fn missing_timestamps_render_as_none() {
        let events = vec![
            started("planner", None),
            transitioned("planner", "coder", None),
        ];
        let p = fold_progress(&events, &def(&["planner", "coder"]), JobStatus::Running, 0);
        assert_eq!(p.agents[0].started_at, None);
        assert_eq!(p.agents[0].ended_at, None);
        assert!(matches!(p.agents[0].phase, AgentPhase::Done));
    }

    #[test]
    fn unvisited_agents_appended_as_pending() {
        let events = vec![started("planner", Some(1))];
        let p = fold_progress(
            &events,
            &def(&["planner", "coder", "reviewer"]),
            JobStatus::Running,
            0,
        );
        assert_eq!(p.agents.len(), 3);
        assert!(matches!(p.agents[0].phase, AgentPhase::Active));
        assert!(matches!(p.agents[1].phase, AgentPhase::Pending));
        assert_eq!(p.agents[1].name, "coder");
        assert!(matches!(p.agents[2].phase, AgentPhase::Pending));
    }

    #[test]
    fn finished_marks_last_done_and_sets_finished_at() {
        let events = vec![
            started("planner", Some(1)),
            transitioned("planner", "coder", Some(2)),
            started("coder", Some(2)),
            finished(Some(9)),
        ];
        let p = fold_progress(&events, &def(&["planner", "coder"]), JobStatus::Finished, 0);
        assert!(matches!(p.agents[1].phase, AgentPhase::Done));
        assert_eq!(p.agents[1].ended_at, Some(9));
        assert_eq!(p.finished_at, Some(9));
    }

    #[test]
    fn failed_sets_finished_at_from_terminal_event() {
        let events = vec![
            started("planner", Some(1)),
            WorkflowDomainEvent::WorkflowFailed {
                error: "boom".into(),
                recoverable: false,
                at_ms: Some(5),
            },
        ];
        let p = fold_progress(&events, &def(&["planner"]), JobStatus::Failed, 0);
        assert!(matches!(p.agents[0].phase, AgentPhase::Done));
        assert_eq!(p.finished_at, Some(5));
    }

    #[test]
    fn pause_keeps_current_agent_active() {
        let events = vec![
            started("planner", Some(1)),
            WorkflowDomainEvent::WorkflowPaused {
                session_id: Uuid::new_v4(),
                tool_call_id: Some("tc".into()),
                at_ms: Some(2),
            },
        ];
        let p = fold_progress(&events, &def(&["planner"]), JobStatus::AwaitingUserInput, 0);
        assert_eq!(p.agents.len(), 1);
        assert!(matches!(p.agents[0].phase, AgentPhase::Active));
        assert_eq!(p.agents[0].ended_at, None);
    }
}
