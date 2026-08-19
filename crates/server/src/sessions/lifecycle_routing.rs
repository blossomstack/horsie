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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::session_actor::runner::event::RunnerArgs;
    use crate::sessions::spec::AgentSettings;
    use uuid::Uuid;

    fn settings() -> AgentSettings {
        AgentSettings {
            model: "mock".into(),
            instructions: None,
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: vec![],
            memory_spaces: vec![],
            thinking_effort: None,
            max_concurrent_subagents: None,
            auto_compact: None,
            control_plane: None,
            plugins: Vec::new(),
        }
    }

    fn graph() -> crate::sessions::workflow::WorkflowRunSpec {
        crate::sessions::workflow::WorkflowRunSpec {
            workflow: "wf".into(),
            start: "s".into(),
            steps: vec![crate::sessions::workflow::WorkflowStepSpec {
                name: "s".into(),
                agent: "preset".into(),
                prompt: "p".into(),
                outcomes: vec![],
                fields: vec![],
                interactive: false,
                transitions: vec![],
                settings: settings(),
            }],
            input: "in".into(),
            max_steps: 100,
        }
    }

    /// Fold a log the way the actor does, so `route` is asked about the state
    /// its event actually produced rather than a hand-built one.
    fn fold(events: Vec<SessionEvent>) -> SessionState {
        let mut state = SessionState::default();
        for e in &events {
            state.apply(e);
        }
        state
    }

    fn created(id: Uuid, parent: Option<Uuid>, args: RunnerArgs) -> SessionEvent {
        SessionEvent::Runner {
            id: RunnerId(id),
            at_ms: 1,
            event: RunnerEvent::Created {
                parent: parent.map(AgentId),
                args: Box::new(args),
            },
        }
    }

    fn sub_args(label: &str) -> RunnerArgs {
        RunnerArgs::Sub {
            label: label.into(),
            task: "t".into(),
            agent_type: None,
            settings: settings(),
        }
    }

    fn fork_args() -> RunnerArgs {
        RunnerArgs::Fork {
            source_seq: 0,
            mode: crate::sessions::forks::ForkMode::Copy,
            message: "go".into(),
        }
    }

    fn step_started(run: Uuid, agent: Uuid) -> SessionEvent {
        SessionEvent::Runner {
            id: RunnerId(run),
            at_ms: 1,
            event: RunnerEvent::StepStarted {
                index: 0,
                step: "s".into(),
                agent: AgentId(agent),
                attempt: 1,
                from: None,
                via: None,
                input: "in".into(),
            },
        }
    }

    fn ended(agent: Uuid, end: RecordedEnd) -> SessionEvent {
        SessionEvent::TurnEnded {
            at_ms: 2,
            agent: AgentId(agent),
            end,
        }
    }

    /// Every variant, listed by hand rather than derived.
    ///
    /// That is the point: adding a variant breaks the compilation of this list
    /// until someone decides where it goes. A forgotten routing is a fact that
    /// silently never reaches a client, which no other test would catch.
    #[test]
    fn every_viewer_facing_event_has_a_destination() {
        let main = Uuid::new_v4();
        let sub = Uuid::new_v4();
        let fork = Uuid::new_v4();
        let run = Uuid::new_v4();
        let step_agent = Uuid::new_v4();
        let conversation = fold(vec![
            created(main, None, RunnerArgs::Main),
            created(sub, Some(main), sub_args("l")),
            created(fork, Some(main), fork_args()),
            created(run, Some(main), RunnerArgs::Workflow { graph: graph() }),
            step_started(run, step_agent),
        ]);
        let renders = |event: SessionEvent| !route(&event, &conversation).is_empty();

        // Session-scoped facts a viewer sees, and the bookkeeping they don't.
        assert!(renders(SessionEvent::ProvisioningStarted { at_ms: 1 }));
        assert!(renders(SessionEvent::ProvisioningProgress {
            at_ms: 1,
            detail: "booting".into()
        }));
        assert!(renders(SessionEvent::ProvisioningSucceeded { at_ms: 1 }));
        assert!(renders(SessionEvent::ProvisioningFailed {
            at_ms: 1,
            error: "no".into(),
            terminal: false
        }));
        assert!(renders(SessionEvent::SessionFailed {
            at_ms: 1,
            reason: "dead".into()
        }));
        assert!(!renders(SessionEvent::SpecRecorded {
            spec: Box::new(crate::sessions::spec::SessionSpec::for_vendor("mock"))
        }));
        assert!(!renders(SessionEvent::Renamed { name: "n".into() }));
        assert!(!renders(SessionEvent::UsageRecorded {
            at_ms: 1,
            agent_id: "main".into(),
            usage_total: crate::agent_loop::UsageTotal::default()
        }));
        assert!(!renders(SessionEvent::TurnBegan {
            at_ms: 1,
            agent: AgentId(main)
        }));

        // Turn ends: every owner kind renders a boundary; an ask is a park the
        // agent already journaled itself.
        assert!(renders(ended(
            main,
            RecordedEnd::Concluded {
                output: serde_json::Value::Null
            }
        )));
        assert!(renders(ended(fork, RecordedEnd::Stopped)));
        assert!(renders(ended(
            sub,
            RecordedEnd::Failed { error: "e".into() }
        )));
        assert!(renders(ended(
            step_agent,
            RecordedEnd::Concluded {
                output: serde_json::Value::Null
            }
        )));
        assert!(!renders(ended(main, RecordedEnd::Asked)));

        // Runner facts. Every creation is news to the agent that asked; the
        // rest of a fork's bookkeeping is the session list's, not a log's.
        let on_runner = |id: Uuid, event: RunnerEvent| SessionEvent::Runner {
            id: RunnerId(id),
            at_ms: 2,
            event,
        };
        assert!(renders(created(sub, Some(main), sub_args("l"))));
        assert!(renders(created(fork, Some(main), fork_args())));
        assert!(renders(created(
            run,
            Some(main),
            RunnerArgs::Workflow { graph: graph() }
        )));
        assert!(renders(step_started(run, step_agent)));
        assert!(renders(on_runner(
            run,
            RunnerEvent::StepCancelled { index: 0 }
        )));
        assert!(renders(on_runner(
            run,
            RunnerEvent::RunFinished {
                output: serde_json::Value::Null
            }
        )));
        assert!(renders(on_runner(
            run,
            RunnerEvent::RunFailed { error: "e".into() }
        )));
        assert!(renders(on_runner(run, RunnerEvent::Cancelled)));
        assert!(!renders(on_runner(sub, RunnerEvent::Reported)));
        assert!(!renders(on_runner(fork, RunnerEvent::ForkSeeded)));
        assert!(!renders(on_runner(
            fork,
            RunnerEvent::ForkSeedFailed { error: "e".into() }
        )));
        assert!(!renders(on_runner(
            fork,
            RunnerEvent::ForkTitled { name: "n".into() }
        )));
        assert!(!renders(on_runner(fork, RunnerEvent::ForkDeleted)));
    }

    /// The vendor's own sentence reaches the log, which is the whole point of
    /// carrying one. Still `Acquiring`, and still no status change: narration
    /// describes the wait, it does not end it.
    #[test]
    fn a_vendors_words_reach_the_log_while_the_runtime_comes_up() {
        let main = Uuid::new_v4();
        let state = fold(vec![created(main, None, RunnerArgs::Main)]);
        let entries = route(
            &SessionEvent::ProvisioningProgress {
                at_ms: 1,
                detail: "the machine is booting".into(),
            },
            &state,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentId(main));
        let LifecycleEvent::Runtime(payload) = &entries[0].1 else {
            panic!("expected a Runtime entry, got {:?}", entries[0].1);
        };
        assert!(matches!(payload.status, RuntimeStatus::Acquiring(_)));
        assert_eq!(payload.detail.as_deref(), Some("the machine is booting"));
    }

    /// The two ends of a provisioning wait carry no detail, and that is not an
    /// oversight; a failure carries the vendor's reason.
    #[test]
    fn provisioning_details_travel_exactly_where_they_exist() {
        let main = Uuid::new_v4();
        let state = fold(vec![created(main, None, RunnerArgs::Main)]);
        for event in [
            SessionEvent::ProvisioningStarted { at_ms: 1 },
            SessionEvent::ProvisioningSucceeded { at_ms: 2 },
        ] {
            let entries = route(&event, &state);
            let Some((_, LifecycleEvent::Runtime(payload))) = entries.first() else {
                panic!("expected a Runtime entry for {event:?}");
            };
            assert_eq!(payload.detail, None, "{event:?}");
        }
        let entries = route(
            &SessionEvent::ProvisioningFailed {
                at_ms: 1,
                error: "no capacity in region".into(),
                terminal: false,
            },
            &state,
        );
        let Some((_, LifecycleEvent::Runtime(payload))) = entries.first() else {
            panic!("expected a Runtime entry");
        };
        assert_eq!(payload.detail.as_deref(), Some("no capacity in region"));
    }

    /// A run that has not started its first step has no log at all yet, so a
    /// session-wide fact in that window is dropped rather than misfiled.
    #[test]
    fn a_run_with_no_step_yet_has_nowhere_to_record() {
        let run = Uuid::new_v4();
        let state = fold(vec![created(
            run,
            None,
            RunnerArgs::Workflow { graph: graph() },
        )]);
        assert!(route(&SessionEvent::ProvisioningStarted { at_ms: 1 }, &state).is_empty());
    }

    /// A spawn and a terminal result both land on the *parent*: that is the
    /// log a person has open while a child is working. A nested child's parent
    /// is the subagent above it, not the main agent.
    #[test]
    fn a_subagents_news_is_recorded_on_its_parent() {
        let main = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let spawn = created(child, Some(parent), sub_args("child"));
        let done = ended(
            child,
            RecordedEnd::Concluded {
                output: "done".into(),
            },
        );
        let state = fold(vec![
            created(main, None, RunnerArgs::Main),
            created(parent, Some(main), sub_args("lead")),
            spawn.clone(),
            done.clone(),
        ]);
        for event in [spawn, done] {
            let entries = route(&event, &state);
            assert_eq!(entries[0].0, AgentId(parent), "{event:?} on the parent");
            let LifecycleEvent::SubAgent(payload) = &entries[0].1 else {
                panic!("expected a SubAgent entry");
            };
            // The label comes off the record, so a terminal result names the
            // child rather than carrying a bare uuid.
            assert_eq!(payload.label, "child");
        }
    }

    /// A run has no main agent, so its step entries go to the step's own log,
    /// named so a person recognises them.
    #[test]
    fn a_runs_steps_are_recorded_on_the_step_that_ran() {
        let run = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let started = step_started(run, agent);
        let concluded = ended(
            agent,
            RecordedEnd::Concluded {
                output: serde_json::Value::Null,
            },
        );
        let state = fold(vec![
            created(run, None, RunnerArgs::Workflow { graph: graph() }),
            started.clone(),
            concluded.clone(),
        ]);
        for event in [started, concluded] {
            let entries = route(&event, &state);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0, AgentId(agent));
            let LifecycleEvent::Step(payload) = &entries[0].1 else {
                panic!("expected a Step entry");
            };
            assert_eq!(payload.name, "s");
        }
    }

    /// A session-wide fact in a run lands on the step in flight; the same fact
    /// in a conversation goes to the main agent.
    #[test]
    fn a_session_wide_fact_lands_on_whatever_the_session_is() {
        let run = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let in_run = fold(vec![
            created(run, None, RunnerArgs::Workflow { graph: graph() }),
            step_started(run, agent),
        ]);
        let entries = route(&SessionEvent::ProvisioningStarted { at_ms: 3 }, &in_run);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentId(agent));

        let main = Uuid::new_v4();
        let conversation = fold(vec![created(main, None, RunnerArgs::Main)]);
        let entries = route(
            &SessionEvent::ProvisioningStarted { at_ms: 3 },
            &conversation,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentId(main));
    }

    /// A spawn under a run's step hangs off that step's agent, so its news
    /// goes to the step and not to a main agent the run does not have.
    #[test]
    fn a_spawn_under_a_step_is_recorded_on_the_step() {
        let run = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let spawn = created(child, Some(agent), sub_args("helper"));
        let state = fold(vec![
            created(run, None, RunnerArgs::Workflow { graph: graph() }),
            step_started(run, agent),
            spawn.clone(),
        ]);
        let entries = route(&spawn, &state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentId(agent));
    }

    /// Every way a conversation's turn can end becomes one entry kind carrying
    /// an outcome, so a consumer asking "is the turn over" does not have to
    /// enumerate the ways it can be.
    #[test]
    fn every_way_a_turn_can_end_becomes_one_entry_kind() {
        let main = Uuid::new_v4();
        let state = fold(vec![created(main, None, RunnerArgs::Main)]);
        let outcomes: Vec<TurnOutcome> = [
            RecordedEnd::Concluded {
                output: serde_json::Value::Null,
            },
            RecordedEnd::Failed {
                error: "boom".into(),
            },
            RecordedEnd::Stopped,
            RecordedEnd::Interrupted,
        ]
        .into_iter()
        .map(|end| {
            match route(&ended(main, end), &state)
                .into_iter()
                .next()
                .map(|(_, ev)| ev)
            {
                Some(LifecycleEvent::TurnEnded(t)) => t.outcome,
                other => panic!("expected TurnEnded, got {other:?}"),
            }
        })
        .collect();
        assert!(matches!(outcomes[0], TurnOutcome::Ended(_)));
        assert!(matches!(outcomes[1], TurnOutcome::Failed(_)));
        assert!(matches!(outcomes[2], TurnOutcome::Stopped(_)));
        assert!(matches!(outcomes[3], TurnOutcome::Interrupted(_)));
    }

    /// A fork's boundary belongs to the fork, never to the conversation it
    /// branched from — and its failure carries the reason, because its own
    /// page is the only place a reader will look for it.
    #[test]
    fn a_forks_turn_boundary_lands_on_that_fork_with_its_error() {
        let main = Uuid::new_v4();
        let fork = Uuid::new_v4();
        let state = fold(vec![
            created(main, None, RunnerArgs::Main),
            created(fork, Some(main), fork_args()),
        ]);
        let entries = route(
            &ended(
                fork,
                RecordedEnd::Failed {
                    error: "the provider said no".into(),
                },
            ),
            &state,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentId(fork));
        let LifecycleEvent::TurnEnded(t) = &entries[0].1 else {
            panic!("expected a TurnEnded entry, got {:?}", entries[0].1);
        };
        let TurnOutcome::Failed(failed) = &t.outcome else {
            panic!("expected a failed outcome, got {:?}", t.outcome);
        };
        assert_eq!(failed.error, "the provider said no");
    }

    /// A terminal runtime failure takes every conversation in the session with
    /// it. A fork or a subagent that never heard would go on believing it may
    /// start a turn, on a runtime that is gone for good.
    #[test]
    fn a_session_failure_reaches_every_agent() {
        let main = Uuid::new_v4();
        let fork = Uuid::new_v4();
        let sub = Uuid::new_v4();
        let state = fold(vec![
            created(main, None, RunnerArgs::Main),
            created(fork, Some(main), fork_args()),
            created(sub, Some(main), sub_args("l")),
        ]);
        let keys: Vec<AgentId> = route(
            &SessionEvent::SessionFailed {
                at_ms: 3,
                reason: "the sandbox is gone".into(),
            },
            &state,
        )
        .into_iter()
        .map(|(key, _)| key)
        .collect();
        assert!(keys.contains(&AgentId(main)), "{keys:?}");
        assert!(keys.contains(&AgentId(fork)), "{keys:?}");
        assert!(keys.contains(&AgentId(sub)), "{keys:?}");
    }

    /// A finished subagent is news in two places: the parent that is waiting
    /// on it, and its own page, which reads `RUNNING` until its turn is
    /// closed.
    #[test]
    fn a_finished_subagent_is_recorded_on_its_parent_and_on_itself() {
        let main = Uuid::new_v4();
        let child = Uuid::new_v4();
        let done = ended(
            child,
            RecordedEnd::Concluded {
                output: "done".into(),
            },
        );
        let state = fold(vec![
            created(main, None, RunnerArgs::Main),
            created(child, Some(main), sub_args("helper")),
            done.clone(),
        ]);
        let entries = route(&done, &state);
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].0, AgentId(main));
        assert!(matches!(entries[0].1, LifecycleEvent::SubAgent(_)));
        assert_eq!(entries[1].0, AgentId(child));
        assert!(matches!(entries[1].1, LifecycleEvent::TurnEnded(_)));
    }

    /// A run an agent invoked reads to its caller like delegated work, because
    /// to the caller it is: the creation is a `SubAgent` entry named after the
    /// workflow, on the caller's own log.
    #[test]
    fn a_nested_runs_creation_is_news_on_the_agent_that_asked() {
        let main = Uuid::new_v4();
        let run = Uuid::new_v4();
        let create = created(run, Some(main), RunnerArgs::Workflow { graph: graph() });
        let state = fold(vec![created(main, None, RunnerArgs::Main), create.clone()]);
        let entries = route(&create, &state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentId(main));
        let LifecycleEvent::SubAgent(payload) = &entries[0].1 else {
            panic!("expected a SubAgent entry, got {:?}", entries[0].1);
        };
        assert_eq!(payload.label, "wf", "named after the workflow it runs");
    }
}
