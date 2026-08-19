//! Which session event reaches which agent's log, and as what.
//!
//! The session actor still owns every one of these — it decides them, journals
//! them, and folds them into its own state. This module only answers "who needs
//! to see it", so that a viewer reading one agent's log sees everything that
//! happened to that agent without a second stream to reconcile against.
//!
//! A pure function with a table of cases, in its own file for the same reason
//! `agent_log` is: the interesting part is the table, and a table wants tests
//! that can enumerate it.

use crate::sessions::session_actor::runner::state::RunnerState;
use crate::sessions::session_actor::{
    AgentId, RecordedEnd, RunnerEvent, RunnerId, SessionEvent, SessionState,
};
use horsie_agentcore::{
    EmptyOutcome, FailedOutcome, ForkLifecycle, LifecycleEvent, RuntimeLifecycle, RuntimeStatus,
    SessionFailedLifecycle, StepLifecycle, SubAgentLifecycle, TurnEndedLifecycle, TurnOutcome,
};

/// One entry: whose log it belongs in, and what it says there.
type Entry = (AgentId, LifecycleEvent);

/// Every log this event belongs in, and what it becomes in each.
///
/// A list rather than one destination, because a fact can matter to more than
/// one reader: a subagent's result is both its own last word and news to the
/// parent that is waiting on it. Bookkeeping returns an empty list, which is
/// how "nothing a viewer would render" is said.
///
/// Takes the state as it stands *after* the event, because some routings are
/// not in the event: a step's entry finds its agent in its run's log.
#[must_use]
pub fn route(event: &SessionEvent, state: &SessionState) -> Vec<Entry> {
    match event {
        SessionEvent::ProvisioningStarted { .. } => on_session(
            state,
            LifecycleEvent::Runtime(RuntimeLifecycle {
                status: RuntimeStatus::Acquiring(EmptyOutcome {}),
                detail: None,
            }),
        ),
        // Still acquiring, now with the vendor's own account of why it is
        // taking as long as it is. The status is unchanged on purpose: this is
        // the same fact as the entry above, said with more of what is known.
        SessionEvent::ProvisioningProgress { detail, .. } => on_session(
            state,
            LifecycleEvent::Runtime(RuntimeLifecycle {
                status: RuntimeStatus::Acquiring(EmptyOutcome {}),
                detail: Some(detail.clone()),
            }),
        ),
        SessionEvent::ProvisioningSucceeded { .. } => on_session(
            state,
            LifecycleEvent::Runtime(RuntimeLifecycle {
                status: RuntimeStatus::Ready(EmptyOutcome {}),
                detail: None,
            }),
        ),
        SessionEvent::ProvisioningFailed { error, .. } => on_session(
            state,
            LifecycleEvent::Runtime(RuntimeLifecycle {
                status: RuntimeStatus::Failed(EmptyOutcome {}),
                detail: Some(error.clone()),
            }),
        ),
        // Every agent, not just the one a person is looking at: this takes the
        // runtime away for good, and a resident agent that never heard would
        // go on believing it may still start a turn.
        SessionEvent::SessionFailed { reason, .. } => every_agent(
            state,
            LifecycleEvent::SessionFailed(SessionFailedLifecycle {
                reason: reason.clone(),
            }),
        ),
        // What an end means — and whose logs hear about it — is the owning
        // runner's kind: a conversation's boundary is its own, a subagent's is
        // also its parent's news, a step's is its run-log entry.
        SessionEvent::TurnEnded { agent, end, .. } => turn_ended(*agent, end, state),
        SessionEvent::Runner { id, event, .. } => runner_event(*id, event, state),
        // Recorded by the agent itself, in its own log, because the agent is
        // what decided it. The session keeps its own copy only to move phase,
        // which is not something a viewer reads off the log.
        SessionEvent::TurnBegan { .. } => Vec::new(),
        // Nothing a reader sees. A usage total is a number on the agent
        // document; a spec and a name are what the session *is*, read from the
        // session document.
        SessionEvent::UsageRecorded { .. }
        | SessionEvent::SpecRecorded { .. }
        | SessionEvent::Renamed { .. } => Vec::new(),
    }
}

fn turn_ended(agent: AgentId, end: &RecordedEnd, state: &SessionState) -> Vec<Entry> {
    let Some(owner) = state.owner_of(agent) else {
        return Vec::new();
    };
    let Some(record) = state.record(owner) else {
        return Vec::new();
    };
    match &record.state {
        // A conversation's boundary, in its own log — the main agent's *is*
        // the session's. An ask is not a boundary: the question is journaled
        // by the agent that asked it.
        RunnerState::Main(_) | RunnerState::Fork(_) => {
            let outcome = match end {
                RecordedEnd::Concluded { .. } => TurnOutcome::Ended(EmptyOutcome {}),
                RecordedEnd::Failed { error } => TurnOutcome::Failed(FailedOutcome {
                    error: error.clone(),
                }),
                RecordedEnd::Stopped => TurnOutcome::Stopped(EmptyOutcome {}),
                RecordedEnd::Interrupted => TurnOutcome::Interrupted(EmptyOutcome {}),
                RecordedEnd::Asked => return Vec::new(),
            };
            vec![(
                agent,
                LifecycleEvent::TurnEnded(TurnEndedLifecycle { outcome }),
            )]
        }
        // A finished subagent, on its parent *and* on itself: the parent is
        // what a person has open while it waits, and the child has a page of
        // its own that would otherwise read `RUNNING` for ever.
        RunnerState::Sub(node) => {
            let (status, outcome) = match end {
                RecordedEnd::Concluded { .. } => ("completed", TurnOutcome::Ended(EmptyOutcome {})),
                RecordedEnd::Failed { error } => (
                    "failed",
                    TurnOutcome::Failed(FailedOutcome {
                        error: error.clone(),
                    }),
                ),
                RecordedEnd::Stopped => (
                    "failed",
                    TurnOutcome::Failed(FailedOutcome {
                        error: crate::sessions::session_actor::runner::STOPPED_ERROR.to_string(),
                    }),
                ),
                RecordedEnd::Asked | RecordedEnd::Interrupted => return Vec::new(),
            };
            let mut entries = Vec::new();
            if let Some(parent) = record.parent {
                entries.push((
                    parent,
                    LifecycleEvent::SubAgent(SubAgentLifecycle {
                        id: agent.to_string(),
                        label: node.label.clone(),
                        status: status.into(),
                    }),
                ));
            }
            entries.push((
                agent,
                LifecycleEvent::TurnEnded(TurnEndedLifecycle { outcome }),
            ));
            entries
        }
        // A step's own log, which for a run is the only one there is.
        RunnerState::Workflow(w) => {
            let Some(index) = w.run.index_of_agent(agent.0) else {
                return Vec::new();
            };
            let status = match end {
                RecordedEnd::Concluded { .. } => "concluded",
                RecordedEnd::Failed { .. } => "failed",
                RecordedEnd::Asked | RecordedEnd::Stopped | RecordedEnd::Interrupted => {
                    return Vec::new();
                }
            };
            step_entry(owner, index, status, state)
        }
    }
}

fn runner_event(id: RunnerId, event: &RunnerEvent, state: &SessionState) -> Vec<Entry> {
    match event {
        // On the parent, not the child: a child appearing is something that
        // happens *in* the parent's trajectory, and the child's own log starts
        // with its own work.
        RunnerEvent::Created { .. } => {
            let Some(record) = state.record(id) else {
                return Vec::new();
            };
            let Some(parent) = record.parent else {
                return Vec::new();
            };
            match &record.state {
                RunnerState::Sub(node) => vec![(
                    parent,
                    LifecycleEvent::SubAgent(SubAgentLifecycle {
                        id: id.to_string(),
                        label: node.label.clone(),
                        status: "running".into(),
                    }),
                )],
                // On the conversation that was forked: a fork of a fork
                // belongs in *that* fork's transcript, where the branch
                // actually happened.
                RunnerState::Fork(f) => vec![(
                    parent,
                    LifecycleEvent::Forked(ForkLifecycle {
                        id: id.to_string(),
                        title: None,
                        mode: f.mode.as_str().to_string(),
                    }),
                )],
                // A run an agent invoked reads to its caller like delegated
                // work, because to the caller it is.
                RunnerState::Workflow(w) => vec![(
                    parent,
                    LifecycleEvent::SubAgent(SubAgentLifecycle {
                        id: id.to_string(),
                        label: w.graph.workflow.clone(),
                        status: "running".into(),
                    }),
                )],
                RunnerState::Main(_) => Vec::new(),
            }
        }
        RunnerEvent::StepStarted {
            index, step, agent, ..
        } => vec![(
            *agent,
            LifecycleEvent::Step(StepLifecycle {
                index: *index,
                name: step.clone(),
                status: "started".into(),
            }),
        )],
        RunnerEvent::StepCancelled { index } => step_entry(id, *index, "cancelled", state),
        // The run's own end, recorded on whichever step last ran — there is no
        // other log to put it in.
        RunnerEvent::RunFinished { .. } => last_step_entry(id, "run_finished", state),
        RunnerEvent::RunFailed { .. } => last_step_entry(id, "run_failed", state),
        RunnerEvent::Cancelled => match current_step(id, state) {
            Some(index) => step_entry(id, index, "cancelled", state),
            None => Vec::new(),
        },
        // Nothing in any transcript changes when a fork is seeded, renames
        // itself or goes; those belong to the session list. A report settling
        // is the session reconciling its own tree.
        RunnerEvent::Reported
        | RunnerEvent::ForkSeeded
        | RunnerEvent::ForkSeedFailed { .. }
        | RunnerEvent::ForkTitled { .. }
        | RunnerEvent::ForkDeleted => Vec::new(),
    }
}

/// A session-wide fact belongs in the log a person reads when they open the
/// session. That is the main agent for a conversation; a run has none, so it
/// goes to the step in flight, whose log is the only one there is.
fn session_wide(state: &SessionState) -> Option<AgentId> {
    let (id, record) = state.root()?;
    match &record.state {
        RunnerState::Main(_) => Some(AgentId(id.0)),
        RunnerState::Workflow(w) => w.run.current_agent().map(AgentId),
        RunnerState::Sub(_) | RunnerState::Fork(_) => None,
    }
}

fn on_session(state: &SessionState, ev: LifecycleEvent) -> Vec<Entry> {
    session_wide(state)
        .map(|key| (key, ev))
        .into_iter()
        .collect()
}

/// One entry on every agent this session hosts. For a fact that changes what
/// an agent may *do*, as opposed to one it merely renders.
fn every_agent(state: &SessionState, ev: LifecycleEvent) -> Vec<Entry> {
    let mut agents: Vec<AgentId> = session_wide(state).into_iter().collect();
    for (id, record) in &state.runners {
        match &record.state {
            RunnerState::Sub(_) | RunnerState::Fork(_) => agents.push(AgentId(id.0)),
            // Every run's step in flight, not only the root's: a nested run's
            // step is as able to start a doomed turn as anyone.
            RunnerState::Workflow(w) => {
                if record.parent.is_some() {
                    agents.extend(w.run.current_agent().map(AgentId));
                }
            }
            RunnerState::Main(_) => {}
        }
    }
    agents.into_iter().map(|key| (key, ev.clone())).collect()
}

/// One step execution's entry, on that step's own agent. The agent and the
/// name both come from the run log, which this event has already been folded
/// into.
fn step_entry(run: RunnerId, index: u32, status: &str, state: &SessionState) -> Vec<Entry> {
    let Some(RunnerState::Workflow(w)) = state.record(run).map(|r| &r.state) else {
        return Vec::new();
    };
    let Some(step) = w.run.get(index) else {
        return Vec::new();
    };
    vec![(
        AgentId(step.agent),
        LifecycleEvent::Step(StepLifecycle {
            index,
            name: step.step.clone(),
            status: status.into(),
        }),
    )]
}

/// The run's own outcome, on the step that ran last.
fn last_step_entry(run: RunnerId, status: &str, state: &SessionState) -> Vec<Entry> {
    let Some(RunnerState::Workflow(w)) = state.record(run).map(|r| &r.state) else {
        return Vec::new();
    };
    let last = w
        .run
        .steps
        .len()
        .checked_sub(1)
        .and_then(|i| u32::try_from(i).ok());
    match last {
        Some(index) => step_entry(run, index, status, state),
        None => Vec::new(),
    }
}

/// The index of the step a run has in flight, if any.
fn current_step(run: RunnerId, state: &SessionState) -> Option<u32> {
    match state.record(run).map(|r| &r.state) {
        Some(RunnerState::Workflow(w)) => w.run.current(),
        _ => None,
    }
}
