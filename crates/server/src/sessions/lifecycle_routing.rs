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

use crate::sessions::run_forest::{RunId, RunState};
use crate::sessions::session_actor::{AgentKey, SessionDomainEvent, SessionState};
use horsie_agentcore::{
    EmptyOutcome, FailedOutcome, LifecycleEvent, RuntimeLifecycle, RuntimeStatus,
    SessionFailedLifecycle, StepLifecycle, SubAgentLifecycle, SubSessionLifecycle,
    TurnEndedLifecycle, TurnOutcome,
};

/// One entry: whose log it belongs in, and what it says there.
type Entry = (AgentKey, LifecycleEvent);

/// Every log this event belongs in, and what it becomes in each.
///
/// A list rather than one destination, because a fact can matter to more than
/// one reader: a subagent's result is both its own last word and news to the
/// parent that is waiting on it. Bookkeeping returns an empty list, which is
/// how "nothing a viewer would render" is said — a usage total is a number,
/// and `SubAgentRunning`/`SubAgentNotified` are the session reconciling its
/// own tree.
///
/// Takes the state as it stands *after* the event, because two of the routings
/// are not in the event: a step execution knows only its index, and a
/// subagent's terminal result knows only its own id. Both find their agent in
/// the run log and the forest respectively.
#[must_use]
pub fn route(event: &SessionDomainEvent, state: &SessionState) -> Vec<Entry> {
    use SessionDomainEvent as E;
    // A session-wide fact belongs in the log a person reads when they open the
    // session. That is the main agent for a session; a run has none, so it
    // goes to the step in flight, whose log is the only one there is.
    let session_wide = |state: &SessionState| -> Option<AgentKey> {
        match state.forest.root_is_workflow() {
            false => Some(AgentKey::Main),
            true => state.forest.current_root_step_agent().map(AgentKey::Step),
        }
    };
    let on_session = |state: &SessionState, ev: LifecycleEvent| -> Vec<Entry> {
        session_wide(state)
            .map(|key| (key, ev))
            .into_iter()
            .collect()
    };
    match event {
        // A runtime being *asked for* reaches no log. What a reader waits on is
        // the create it starts, which the next event says — and a sub session
        // that asked for its own has not been told anything yet.
        E::RuntimeRequested { .. } => Vec::new(),
        E::ProvisioningStarted { .. } => on_session(
            state,
            LifecycleEvent::Runtime(RuntimeLifecycle {
                status: RuntimeStatus::Acquiring(EmptyOutcome {}),
                detail: None,
            }),
        ),
        // Still acquiring, now with the vendor's own account of why it is
        // taking as long as it is. The status is unchanged on purpose: this is
        // the same fact as the entry above, said with more of what is known.
        E::ProvisioningProgress { detail, .. } => on_session(
            state,
            LifecycleEvent::Runtime(RuntimeLifecycle {
                status: RuntimeStatus::Acquiring(EmptyOutcome {}),
                detail: Some(detail.clone()),
            }),
        ),
        E::ProvisioningSucceeded { .. } => on_session(
            state,
            LifecycleEvent::Runtime(RuntimeLifecycle {
                status: RuntimeStatus::Ready(EmptyOutcome {}),
                // Nothing to add: the runtime is up, which is the whole
                // message. A detail here would be narration of a wait that is
                // over.
                detail: None,
            }),
        ),
        E::ProvisioningFailed { error, .. } => on_session(
            state,
            LifecycleEvent::Runtime(RuntimeLifecycle {
                status: RuntimeStatus::Failed(EmptyOutcome {}),
                detail: Some(error.clone()),
            }),
        ),
        E::TurnEnded { .. } => on_session(
            state,
            LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: TurnOutcome::Ended(EmptyOutcome {}),
            }),
        ),
        E::TurnFailed { error, .. } => on_session(
            state,
            LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: TurnOutcome::Failed(FailedOutcome {
                    error: error.clone(),
                }),
            }),
        ),
        E::TurnStopped { .. } => on_session(
            state,
            LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: TurnOutcome::Stopped(EmptyOutcome {}),
            }),
        ),
        E::TurnInterrupted { .. } => on_session(
            state,
            LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: TurnOutcome::Interrupted(EmptyOutcome {}),
            }),
        ),
        // Every agent, not just the one a person is looking at: this takes the
        // runtime away for good, and a resident subagent that never heard would
        // go on believing it may still start a turn.
        E::SessionFailed { reason, .. } => every_agent(
            state,
            LifecycleEvent::SessionFailed(SessionFailedLifecycle {
                reason: reason.clone(),
            }),
        ),
        // On the parent, not the child: a subagent appearing is something that
        // happens *in* the parent's trajectory, and the child's own log starts
        // with its own work.
        E::SubAgentSpawned {
            id, parent, label, ..
        } => parent_key(*parent, state)
            .map(|key| {
                (
                    key,
                    LifecycleEvent::SubAgent(SubAgentLifecycle {
                        id: id.to_string(),
                        label: label.clone(),
                        status: "running".into(),
                    }),
                )
            })
            .into_iter()
            .collect(),
        // On the parent too, and for the same reason the spawn is: the parent
        // is what a person has open while it waits. It used to reach only the
        // child, whose own log already ends with the report.
        E::SubAgentCompleted { id, .. } => terminal_subagent(*id, None, state),
        E::SubAgentFailed { id, error, .. } => terminal_subagent(*id, Some(error), state),
        // A step's own log, which for a run is the only one there is. These
        // used to route to `Main`, which a run does not have, so every one of
        // them was dropped with a warning.
        E::StepStarted {
            index, step, agent, ..
        } => vec![(
            AgentKey::Step(*agent),
            LifecycleEvent::Step(StepLifecycle {
                index: *index,
                name: step.clone(),
                status: "started".into(),
            }),
        )],
        E::StepConcluded { run, index, .. } => step_entry(RunId(*run), *index, "concluded", state),
        E::StepFailed { run, index, .. } => step_entry(RunId(*run), *index, "failed", state),
        E::StepCancelled { run, index, .. } => step_entry(RunId(*run), *index, "cancelled", state),
        // The run's own end: on whichever of its steps last ran — there is no
        // other log to put it in — and, for an invoked run, on the agent that
        // invoked it, in the delegated-work vocabulary that agent's own gate
        // reads.
        E::RunFinished { run, .. } => {
            let mut out = last_step_entry(RunId(*run), "run_finished", state);
            out.extend(run_report_entry(RunId(*run), "completed", state));
            out
        }
        E::RunFailed { run, .. } => {
            let mut out = last_step_entry(RunId(*run), "run_failed", state);
            out.extend(run_report_entry(RunId(*run), "failed", state));
            out
        }
        // A run an agent invoked appearing is something that happens *in* that
        // agent's trajectory — the same rule a subagent spawn follows.
        E::RunCreated {
            id, parent, graph, ..
        } => parent_key(*parent, state)
            .map(|key| {
                (
                    key,
                    LifecycleEvent::SubAgent(SubAgentLifecycle {
                        id: id.to_string(),
                        label: format!("workflow {}", graph.workflow),
                        status: "running".into(),
                    }),
                )
            })
            .into_iter()
            .collect(),
        // The session reconciling its own ledger; the report itself reaches
        // the parent as a queued part.
        E::RunNotified { .. } => Vec::new(),
        // Bookkeeping, deliberately not surfaced. A usage total is a number on
        // the agent document; `SubAgentRunning` and `SubAgentNotified` are the
        // session reconciling its own tree, and a viewer already sees the
        // spawn and the result.
        // Nothing a reader sees. A spec and a name are what the session *is*,
        // not something that happened in it — the client reads them from the
        // session document, and a transcript entry would only repeat it.
        E::UsageRecorded { .. }
        | E::SubAgentRunning { .. }
        | E::SubAgentNotified { .. }
        | E::SpecRecorded { .. }
        | E::Renamed { .. } => Vec::new(),
        // On the session that was branched, not in the session-wide log: a sub
        // session of a sub session belongs in *that* sub session's transcript,
        // where the branch actually happened. The same rule
        // `SubAgentLifecycle` follows.
        //
        // It never reaches the model — `prompt_messages` drops every lifecycle
        // body — which is deliberate: a sub session is for the person reading,
        // and telling the source about it would disturb its prompt cache for
        // nothing.
        E::SubSessionCreated {
            id, parent, seed, ..
        } => parent_key(*parent, state)
            .map(|key| {
                (
                    key,
                    LifecycleEvent::SubSession(SubSessionLifecycle {
                        id: id.to_string(),
                        title: None,
                        seed: seed.as_str().to_string(),
                    }),
                )
            })
            .into_iter()
            .collect(),
        // Nothing in the source's transcript changes when a sub session is
        // seeded, renames itself, moves or goes. Those belong in the sub
        // session's own log, and the session list is where a reader watches
        // them.
        E::SubSessionSeeded { .. }
        | E::SubSessionTitled { .. }
        | E::SubSessionStatusChanged { .. }
        | E::SubSessionDeleted { .. } => Vec::new(),
        // On the sub session itself, not on the session it branched from: this
        // is the boundary of the sub session's *own* turn, and a page folds a
        // `TurnBegan` as `Running` until it sees the matching end. Left out, a
        // sub session read `RUNNING` for ever — through reloads and restarts,
        // because the status is derived from the journal.
        E::SubSessionTurnEnded { id, outcome, .. } => vec![(
            AgentKey::SubSession(*id),
            LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: outcome.clone(),
            }),
        )],
        // Recorded by the agent itself, in its own log, because the agent is
        // what decided them. Routing them from here as well would render the
        // same fact twice. The session keeps its own copy only to move
        // `status`, which is not something a viewer reads off the log.
        E::TurnBegan { .. } | E::AskRecorded { .. } => Vec::new(),
    }
}

/// One entry on every agent this session hosts — the session-wide one plus
/// every node in the forest. For a fact that changes what an agent may *do*,
/// as opposed to one it merely renders.
fn every_agent(state: &SessionState, ev: LifecycleEvent) -> Vec<Entry> {
    let session_wide = match state.forest.root_is_workflow() {
        false => Some(AgentKey::Main),
        true => state.forest.current_root_step_agent().map(AgentKey::Step),
    };
    session_wide
        .into_iter()
        .chain(state.forest.sub_ids().into_iter().map(AgentKey::Sub))
        // Sub sessions too. `runtime_readiness` is what reads a
        // `SessionFailed` off an agent's log and stops it starting another
        // turn; a sub session that never heard would go on believing it may
        // run, on a runtime that is gone.
        .chain(
            state
                .forest
                .sub_session_ids()
                .into_iter()
                .map(AgentKey::SubSession),
        )
        .map(|key| (key, ev.clone()))
        .collect()
}

/// A finished subagent, on its parent *and* on itself.
///
/// On the parent because that is what a person has open while it waits, and the
/// label comes off the forest — the event carries only the id, and a bare uuid
/// is not something a reader can place.
///
/// On the child because a subagent has a page of its own, and a page folds
/// `TurnBegan` as `Running` until the matching end. The child's log used to get
/// nothing, so a finished subagent read `RUNNING` for ever there while the
/// forest beside it said `completed` — the same defect a step had before its
/// `Step` entries were folded, and a sub session had before `ForkTurnEnded`.
fn terminal_subagent(id: uuid::Uuid, error: Option<&String>, state: &SessionState) -> Vec<Entry> {
    let Some(record) = state.forest.sub(id) else {
        return Vec::new();
    };
    let outcome = match error {
        Some(error) => TurnOutcome::Failed(FailedOutcome {
            error: error.clone(),
        }),
        None => TurnOutcome::Ended(EmptyOutcome {}),
    };
    let parent = state
        .forest
        .owner_of_agent(id)
        .and_then(|(_, e)| e.parent)
        .and_then(|p| parent_key(p, state));
    parent
        .map(|key| {
            (
                key,
                LifecycleEvent::SubAgent(SubAgentLifecycle {
                    id: id.to_string(),
                    label: record.label.clone(),
                    status: match error {
                        Some(_) => "failed".into(),
                        None => "completed".into(),
                    },
                }),
            )
        })
        .into_iter()
        .chain(std::iter::once((
            AgentKey::Sub(id),
            LifecycleEvent::TurnEnded(TurnEndedLifecycle { outcome }),
        )))
        .collect()
}

/// One step execution's entry, on that step's own agent. The agent and the name
/// both come from the run's log, which this event has already been folded into.
fn step_entry(run: RunId, index: u32, status: &str, state: &SessionState) -> Vec<Entry> {
    let Some(step) = state.forest.workflow(run).and_then(|w| w.run.get(index)) else {
        return Vec::new();
    };
    vec![(
        AgentKey::Step(step.agent),
        LifecycleEvent::Step(StepLifecycle {
            index,
            name: step.step.clone(),
            status: status.into(),
        }),
    )]
}

/// One run's own outcome, on the step of it that ran last.
fn last_step_entry(run: RunId, status: &str, state: &SessionState) -> Vec<Entry> {
    let Some(w) = state.forest.workflow(run) else {
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

/// An invoked run's terminal report, on the agent that invoked it — the same
/// `SubAgent` vocabulary a subagent's terminal entry uses, which is what lets
/// the invoker's own outstanding-work gate read runs and subagents alike.
fn run_report_entry(run: RunId, status: &str, state: &SessionState) -> Vec<Entry> {
    let Some(entry) = state.forest.entry(run) else {
        return Vec::new();
    };
    let Some(parent) = entry.parent else {
        return Vec::new();
    };
    let (RunState::Workflow(w), Some(key)) = (&entry.state, parent_key(parent, state)) else {
        return Vec::new();
    };
    vec![(
        key,
        LifecycleEvent::SubAgent(SubAgentLifecycle {
            id: run.0.to_string(),
            label: format!("workflow {}", w.workflow),
            status: status.into(),
        }),
    )]
}

/// Whose log a child's news belongs in: the agent it runs under, keyed as what
/// that agent is. `None` for an agent the forest no longer knows.
fn parent_key(parent: uuid::Uuid, state: &SessionState) -> Option<AgentKey> {
    let (_, entry) = state.forest.owner_of_agent(parent)?;
    Some(match &entry.state {
        RunState::Main(_) => AgentKey::Main,
        RunState::Sub(_) => AgentKey::Sub(parent),
        RunState::Workflow(_) => AgentKey::Step(parent),
        RunState::SubSession(_) => AgentKey::SubSession(parent),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::run_forest::SeedMode;
    use crate::sessions::session_actor::SessionActor;
    use horsie_actor::EventSourcedActor;
    use std::sync::Arc;
    use uuid::Uuid;

    /// Fold a log the way the actor does, so `route` is asked about the state
    /// its event actually produced rather than a hand-built one.
    fn fold(events: Vec<SessionDomainEvent>) -> SessionState {
        events
            .into_iter()
            .fold(SessionState::default(), SessionActor::apply_event)
    }

    /// The event that roots a session: every routing that resolves an
    /// agent resolves it through the forest now, and the forest is rooted by
    /// the spec.
    fn session_root(session: Uuid) -> SessionDomainEvent {
        SessionDomainEvent::SpecRecorded {
            at_ms: 0,
            session,
            spec: Box::new(crate::sessions::spec::SessionSpec::for_vendor("mock")),
        }
    }

    fn workflow_root(session: Uuid) -> SessionDomainEvent {
        let mut spec = crate::sessions::spec::SessionSpec::for_vendor("mock");
        spec.kind = crate::sessions::spec::SessionKind::Workflow {
            run: Arc::new(crate::sessions::workflow::WorkflowRunSpec {
                workflow: "w".into(),
                start: "s".into(),
                steps: vec![],
                input: "in".into(),
                max_steps: 10,
            }),
        };
        SessionDomainEvent::SpecRecorded {
            at_ms: 0,
            session,
            spec: Box::new(spec),
        }
    }

    /// Every variant, listed by hand rather than derived.
    ///
    /// That is the point: adding a `SessionDomainEvent` breaks the compilation
    /// of this list until someone decides where it goes. A forgotten routing is
    /// a fact that silently never reaches a client, which no other test would
    /// catch — the session would journal it, fold it, and nobody would ever see
    /// it.
    /// One fixed runtime id for the whole list: which runtime a provisioning
    /// event names does not change where it is routed, and a fresh id per event
    /// would only make the list harder to read.
    fn rt() -> crate::sessions::spec::RuntimeId {
        crate::sessions::spec::RuntimeId(Uuid::from_bytes([7; 16]))
    }

    fn every_variant(session: Uuid, sub: Uuid, step_agent: Uuid) -> Vec<SessionDomainEvent> {
        use SessionDomainEvent as E;
        vec![
            E::RuntimeRequested {
                at_ms: 1,
                runtime: rt(),
                owner: session,
                env: crate::sessions::spec::SessionSpec::for_vendor("mock")
                    .runtime_env()
                    .expect("a vendor spec has a runtime"),
            },
            E::ProvisioningStarted {
                at_ms: 1,
                runtime: rt(),
            },
            E::ProvisioningProgress {
                at_ms: 1,
                runtime: rt(),
                detail: "the machine is booting".into(),
            },
            E::ProvisioningSucceeded {
                at_ms: 1,
                runtime: rt(),
            },
            E::ProvisioningFailed {
                at_ms: 1,
                runtime: rt(),
                error: "no".into(),
                terminal: false,
            },
            E::TurnBegan {
                at_ms: 1,
                agent: session,
            },
            E::AskRecorded {
                at_ms: 1,
                agent: session,
            },
            E::TurnEnded {
                at_ms: 1,
                agent: session,
            },
            E::TurnFailed {
                at_ms: 1,
                agent: session,
                error: "boom".into(),
            },
            E::TurnStopped {
                at_ms: 1,
                agent: session,
            },
            E::TurnInterrupted {
                at_ms: 1,
                agent: session,
            },
            E::SessionFailed {
                at_ms: 1,
                reason: "dead".into(),
            },
            E::UsageRecorded {
                at_ms: 1,
                agent_id: "main".into(),
                usage_total: crate::agent_loop::UsageTotal::default(),
            },
            E::SubAgentSpawned {
                at_ms: 1,
                id: sub,
                parent: session,
                label: "l".into(),
                task: "t".into(),
                agent_type: None,
            },
            E::SubAgentRunning { at_ms: 1, id: sub },
            E::SubAgentCompleted {
                at_ms: 1,
                id: sub,
                output: "done".into(),
            },
            E::SubAgentFailed {
                at_ms: 1,
                id: sub,
                error: "no".into(),
            },
            E::SubAgentNotified { at_ms: 1, id: sub },
            E::RunCreated {
                at_ms: 1,
                id: Uuid::new_v4(),
                parent: session,
                graph: Arc::new(crate::sessions::workflow::WorkflowRunSpec {
                    workflow: "w".into(),
                    start: "s".into(),
                    steps: vec![],
                    input: "in".into(),
                    max_steps: 10,
                }),
            },
            E::StepStarted {
                at_ms: 1,
                run: session,
                index: 0,
                step: "s".into(),
                agent: step_agent,
                attempt: 1,
                from: None,
                via: None,
                input: "in".into(),
            },
            E::StepConcluded {
                at_ms: 1,
                run: session,
                index: 0,
                output: serde_json::Value::Null,
            },
            E::StepFailed {
                at_ms: 1,
                run: session,
                index: 0,
                error: "no".into(),
            },
            E::StepCancelled {
                at_ms: 1,
                run: session,
                index: 0,
            },
            E::RunFinished {
                at_ms: 1,
                run: session,
                output: serde_json::Value::Null,
            },
            E::RunFailed {
                at_ms: 1,
                run: session,
                error: "no".into(),
            },
            E::RunNotified {
                at_ms: 1,
                run: session,
            },
        ]
    }

    /// Whether this event is part of a workflow run, and so has to be routed
    /// against a run's state rather than a session's.
    fn is_run_event(event: &SessionDomainEvent) -> bool {
        use SessionDomainEvent as E;
        matches!(
            event,
            E::StepStarted { .. }
                | E::StepConcluded { .. }
                | E::StepFailed { .. }
                | E::StepCancelled { .. }
                | E::RunFinished { .. }
                | E::RunFailed { .. }
        )
    }

    /// Bookkeeping routes nowhere; everything else routes somewhere.
    ///
    /// "Bookkeeping" covers `TurnBegan` and `AskRecorded` — recorded by the
    /// agent that decided them, in its own log — plus the session reconciling
    /// its own ledger (`UsageRecorded`, `SubAgentRunning`, the two notified
    /// marks).
    ///
    /// Each event is asked against the state it would really occur in — a run's
    /// step events against a run mid-step, everything else against a
    /// session — because several of the routings resolve their agent from
    /// state rather than from the event.
    #[test]
    fn every_viewer_facing_event_has_a_destination() {
        let session = Uuid::new_v4();
        let sub = Uuid::new_v4();
        let step_agent = Uuid::new_v4();
        let plain = fold(vec![
            session_root(session),
            SessionDomainEvent::SubAgentSpawned {
                at_ms: 1,
                id: sub,
                parent: session,
                label: "l".into(),
                task: "t".into(),
                agent_type: None,
            },
        ]);
        let run = fold(vec![
            workflow_root(session),
            step_context_for(session, step_agent),
        ]);
        for event in every_variant(session, sub, step_agent) {
            let bookkeeping = matches!(
                event,
                // Asking for a runtime is not a fact a reader waits on — the
                // create it starts is, and that is the next event.
                SessionDomainEvent::RuntimeRequested { .. }
                    | SessionDomainEvent::UsageRecorded { .. }
                    | SessionDomainEvent::SubAgentRunning { .. }
                    | SessionDomainEvent::SubAgentNotified { .. }
                    | SessionDomainEvent::RunNotified { .. }
                    | SessionDomainEvent::TurnBegan { .. }
                    | SessionDomainEvent::AskRecorded { .. }
            );
            let state = match is_run_event(&event) {
                true => &run,
                false => &plain,
            };
            let entries = route(&event, state);
            match bookkeeping {
                true => assert!(entries.is_empty(), "{event:?} is bookkeeping"),
                false => assert!(!entries.is_empty(), "{event:?} has no destination"),
            }
        }
    }

    /// The vendor's own sentence reaches the log, which is the whole point of
    /// carrying one: "provisioning" for four minutes says nothing, while "the
    /// machine is resuming" is the answer to what a person is waiting for.
    #[test]
    fn a_vendors_words_reach_the_log_while_the_runtime_comes_up() {
        let state = fold(vec![session_root(Uuid::new_v4())]);
        let entries = route(
            &SessionDomainEvent::ProvisioningProgress {
                at_ms: 1,
                runtime: rt(),
                detail: "the machine is booting".into(),
            },
            &state,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentKey::Main);
        let LifecycleEvent::Runtime(payload) = &entries[0].1 else {
            panic!("expected a Runtime entry, got {:?}", entries[0].1);
        };
        assert!(matches!(payload.status, RuntimeStatus::Acquiring(_)));
        assert_eq!(payload.detail.as_deref(), Some("the machine is booting"));
    }

    /// The two ends of a provisioning wait carry no detail, and that is not an
    /// oversight: "acquiring" is the start of a wait nothing is known about
    /// yet, and "ready" is the end of one, where the only news is that it is
    /// over.
    #[test]
    fn the_ends_of_a_provisioning_wait_have_nothing_to_say() {
        let state = fold(vec![session_root(Uuid::new_v4())]);
        for event in [
            SessionDomainEvent::ProvisioningStarted {
                at_ms: 1,
                runtime: rt(),
            },
            SessionDomainEvent::ProvisioningSucceeded {
                at_ms: 2,
                runtime: rt(),
            },
        ] {
            let entries = route(&event, &state);
            let Some((_, LifecycleEvent::Runtime(payload))) = entries.first() else {
                panic!("expected a Runtime entry for {event:?}");
            };
            assert_eq!(payload.detail, None, "{event:?}");
        }
    }

    /// A failure carries the vendor's reason, which is the one detail that was
    /// never dropped — and the one this variant must keep.
    #[test]
    fn a_failed_provision_reports_why() {
        let entries = route(
            &SessionDomainEvent::ProvisioningFailed {
                at_ms: 1,
                runtime: rt(),
                error: "no capacity in region".into(),
                terminal: false,
            },
            &fold(vec![session_root(Uuid::new_v4())]),
        );
        let Some((_, LifecycleEvent::Runtime(payload))) = entries.first() else {
            panic!("expected a Runtime entry");
        };
        assert_eq!(payload.detail.as_deref(), Some("no capacity in region"));
    }

    /// A run that has not started its first step has no log at all yet, so a
    /// session-wide fact in that window is dropped rather than misfiled. This
    /// is the one case with genuinely nowhere to go: step agents are spawned
    /// per step, and a run has no main agent to fall back on.
    #[test]
    fn a_run_with_no_step_yet_has_nowhere_to_record() {
        let state = fold(vec![workflow_root(Uuid::new_v4())]);
        assert!(
            route(
                &SessionDomainEvent::ProvisioningStarted {
                    at_ms: 1,
                    runtime: rt()
                },
                &state
            )
            .is_empty()
        );
    }

    /// A spawn and a terminal result both land on the *parent*: that is the log
    /// a person has open while a child is working. A nested child's parent is
    /// the subagent above it, not the main agent.
    #[test]
    fn a_subagents_news_is_recorded_on_its_parent() {
        let session = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let spawn = SessionDomainEvent::SubAgentSpawned {
            at_ms: 1,
            id: child,
            parent,
            label: "child".into(),
            task: "t".into(),
            agent_type: None,
        };
        let done = SessionDomainEvent::SubAgentCompleted {
            at_ms: 2,
            id: child,
            output: "done".into(),
        };
        let state = fold(vec![
            session_root(session),
            SessionDomainEvent::SubAgentSpawned {
                at_ms: 0,
                id: parent,
                parent: session,
                label: "lead".into(),
                task: "t".into(),
                agent_type: None,
            },
            spawn.clone(),
            done.clone(),
        ]);
        for event in [spawn, done] {
            let entries = route(&event, &state);
            assert_eq!(
                entries[0].0,
                AgentKey::Sub(parent),
                "{event:?} on the parent"
            );
            let LifecycleEvent::SubAgent(payload) = &entries[0].1 else {
                panic!("expected a SubAgent entry");
            };
            // The label comes off the forest, so a terminal result names the
            // child rather than carrying a bare uuid.
            assert_eq!(payload.label, "child");
        }
    }

    /// A run has no main agent, so its step entries go to the step's own log.
    /// They used to name `Main` and be dropped with a warning — every one of
    /// them, for the whole life of the run.
    #[test]
    fn a_runs_steps_are_recorded_on_the_step_that_ran() {
        let session = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let started = SessionDomainEvent::StepStarted {
            at_ms: 1,
            run: session,
            index: 0,
            step: "review".into(),
            agent,
            attempt: 1,
            from: None,
            via: None,
            input: "go".into(),
        };
        let concluded = SessionDomainEvent::StepConcluded {
            at_ms: 2,
            run: session,
            index: 0,
            output: serde_json::Value::Null,
        };
        let state = fold(vec![
            workflow_root(session),
            started.clone(),
            concluded.clone(),
        ]);
        for event in [started, concluded] {
            let entries = route(&event, &state);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0, AgentKey::Step(agent));
            let LifecycleEvent::Step(payload) = &entries[0].1 else {
                panic!("expected a Step entry");
            };
            // The name, not just the index: an index identifies the execution,
            // the name is what a person recognises.
            assert_eq!(payload.name, "review");
        }
    }

    /// An invoked run's terminal report lands on the agent that invoked it, in
    /// the same delegated-work vocabulary a subagent's report uses.
    #[test]
    fn an_invoked_runs_end_is_recorded_on_its_invoker() {
        let session = Uuid::new_v4();
        let run = Uuid::new_v4();
        let created = SessionDomainEvent::RunCreated {
            at_ms: 1,
            id: run,
            parent: session,
            graph: Arc::new(crate::sessions::workflow::WorkflowRunSpec {
                workflow: "deploy".into(),
                start: "s".into(),
                steps: vec![],
                input: "in".into(),
                max_steps: 10,
            }),
        };
        let finished = SessionDomainEvent::RunFinished {
            at_ms: 2,
            run,
            output: serde_json::Value::Null,
        };
        let state = fold(vec![
            session_root(session),
            created.clone(),
            finished.clone(),
        ]);
        let entries = route(&created, &state);
        assert_eq!(entries[0].0, AgentKey::Main);
        let LifecycleEvent::SubAgent(payload) = &entries[0].1 else {
            panic!("expected a SubAgent entry, got {:?}", entries[0].1);
        };
        assert_eq!(payload.label, "workflow deploy");
        assert_eq!(payload.status, "running");
        let entries = route(&finished, &state);
        assert!(
            entries.iter().any(|(key, ev)| *key == AgentKey::Main
                && matches!(ev, LifecycleEvent::SubAgent(p) if p.status == "completed")),
            "the invoker hears the run finished: {entries:?}"
        );
    }

    /// A session-wide fact in a run has nowhere else to go either, so it lands
    /// on the step in flight rather than on a main agent that does not exist.
    #[test]
    fn a_session_wide_fact_in_a_run_lands_on_the_step_in_flight() {
        let session = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let state = fold(vec![
            workflow_root(session),
            step_context_for(session, agent),
        ]);
        let entries = route(
            &SessionDomainEvent::TurnEnded {
                at_ms: 3,
                agent: session,
            },
            &state,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentKey::Step(agent));
    }

    /// The same fact in a session goes to the main agent.
    #[test]
    fn a_session_wide_fact_in_a_session_lands_on_main() {
        let session = Uuid::new_v4();
        let state = fold(vec![session_root(session)]);
        let entries = route(
            &SessionDomainEvent::TurnEnded {
                at_ms: 3,
                agent: session,
            },
            &state,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentKey::Main);
    }

    /// A spawn under a run's step hangs off the step agent, so its news goes to
    /// the step and not to a main agent the run does not have.
    #[test]
    fn a_spawn_under_a_step_is_recorded_on_the_step() {
        let session = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let spawn = SessionDomainEvent::SubAgentSpawned {
            at_ms: 2,
            id: child,
            parent: agent,
            label: "helper".into(),
            task: "t".into(),
            agent_type: None,
        };
        let state = fold(vec![
            workflow_root(session),
            step_context_for(session, agent),
            spawn.clone(),
        ]);
        let entries = route(&spawn, &state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentKey::Step(agent));
    }

    /// Four session events collapse into one lifecycle entry carrying an
    /// outcome, so a consumer asking "is the turn over" does not have to
    /// enumerate the ways it can be.
    #[test]
    fn every_way_a_turn_can_end_becomes_one_entry_kind() {
        let session = Uuid::new_v4();
        let state = fold(vec![session_root(session)]);
        let outcomes: Vec<TurnOutcome> = [
            SessionDomainEvent::TurnEnded {
                at_ms: 1,
                agent: session,
            },
            SessionDomainEvent::TurnFailed {
                at_ms: 1,
                agent: session,
                error: "boom".into(),
            },
            SessionDomainEvent::TurnStopped {
                at_ms: 1,
                agent: session,
            },
            SessionDomainEvent::TurnInterrupted {
                at_ms: 1,
                agent: session,
            },
        ]
        .iter()
        .map(
            |e| match route(e, &state).into_iter().next().map(|(_, ev)| ev) {
                Some(LifecycleEvent::TurnEnded(t)) => t.outcome,
                other => panic!("expected TurnEnded, got {other:?}"),
            },
        )
        .collect();
        assert!(matches!(outcomes[0], TurnOutcome::Ended(_)));
        assert!(matches!(outcomes[1], TurnOutcome::Failed(_)));
        assert!(matches!(outcomes[2], TurnOutcome::Stopped(_)));
        assert!(matches!(outcomes[3], TurnOutcome::Interrupted(_)));
    }

    /// A sub session's boundary belongs to the sub session, never to the
    /// session it branched from: routed session-wide it would close the *main*
    /// agent's turn and leave the sub session reading `RUNNING`.
    #[test]
    fn a_sub_sessions_turn_boundary_lands_on_that_sub_session() {
        let session = Uuid::new_v4();
        let sub_session = Uuid::new_v4();
        let state = fold(vec![
            session_root(session),
            sub_session_context_for(session, sub_session),
        ]);
        let entries = route(
            &SessionDomainEvent::SubSessionTurnEnded {
                at_ms: 2,
                id: sub_session,
                outcome: TurnOutcome::Ended(EmptyOutcome {}),
            },
            &state,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentKey::SubSession(sub_session));
        assert!(matches!(entries[0].1, LifecycleEvent::TurnEnded(_)));
    }

    /// A sub session's turn can fail, and the reason has to survive the trip:
    /// its own page is the only place a reader will look for it.
    #[test]
    fn a_sub_sessions_failed_turn_carries_its_error() {
        let session = Uuid::new_v4();
        let sub_session = Uuid::new_v4();
        let state = fold(vec![
            session_root(session),
            sub_session_context_for(session, sub_session),
        ]);
        let entries = route(
            &SessionDomainEvent::SubSessionTurnEnded {
                at_ms: 2,
                id: sub_session,
                outcome: TurnOutcome::Failed(FailedOutcome {
                    error: "the provider said no".into(),
                }),
            },
            &state,
        );
        let LifecycleEvent::TurnEnded(ended) = &entries[0].1 else {
            panic!("expected a TurnEnded entry, got {:?}", entries[0].1);
        };
        let TurnOutcome::Failed(failed) = &ended.outcome else {
            panic!("expected a failed outcome, got {:?}", ended.outcome);
        };
        assert_eq!(failed.error, "the provider said no");
    }

    /// A terminal runtime failure takes every session in the session with it.
    /// A sub session that never heard would go on believing it may start a
    /// turn, on a runtime that is gone for good.
    #[test]
    fn a_session_failure_reaches_the_sub_sessions_too() {
        let session = Uuid::new_v4();
        let sub_session = Uuid::new_v4();
        let state = fold(vec![
            session_root(session),
            sub_session_context_for(session, sub_session),
        ]);
        let keys: Vec<AgentKey> = route(
            &SessionDomainEvent::SessionFailed {
                at_ms: 3,
                reason: "the sandbox is gone".into(),
            },
            &state,
        )
        .into_iter()
        .map(|(key, _)| key)
        .collect();
        assert!(keys.contains(&AgentKey::Main), "{keys:?}");
        assert!(
            keys.contains(&AgentKey::SubSession(sub_session)),
            "{keys:?}"
        );
    }

    /// A finished subagent is news in two places: the parent that is waiting on
    /// it, and its own page, which reads `RUNNING` until its turn is closed.
    #[test]
    fn a_finished_subagent_is_recorded_on_its_parent_and_on_itself() {
        let session = Uuid::new_v4();
        let child = Uuid::new_v4();
        let state = fold(vec![
            session_root(session),
            SessionDomainEvent::SubAgentSpawned {
                at_ms: 1,
                id: child,
                parent: session,
                label: "helper".into(),
                task: "t".into(),
                agent_type: None,
            },
            SessionDomainEvent::SubAgentCompleted {
                at_ms: 2,
                id: child,
                output: "done".into(),
            },
        ]);
        let entries = route(
            &SessionDomainEvent::SubAgentCompleted {
                at_ms: 2,
                id: child,
                output: "done".into(),
            },
            &state,
        );
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].0, AgentKey::Main);
        assert!(matches!(entries[0].1, LifecycleEvent::SubAgent(_)));
        assert_eq!(entries[1].0, AgentKey::Sub(child));
        assert!(matches!(entries[1].1, LifecycleEvent::TurnEnded(_)));
    }

    fn sub_session_context_for(session: Uuid, sub_session: Uuid) -> SessionDomainEvent {
        SessionDomainEvent::SubSessionCreated {
            at_ms: 1,
            id: sub_session,
            parent: session,
            source_seq: 0,
            seed: SeedMode::Copy,
            runtime: crate::sessions::run_forest::RuntimeChoice::Inherit,
            message: "go".into(),
        }
    }

    fn step_context_for(session: Uuid, agent: Uuid) -> SessionDomainEvent {
        SessionDomainEvent::StepStarted {
            at_ms: 1,
            run: session,
            index: 0,
            step: "s".into(),
            agent,
            attempt: 1,
            from: None,
            via: None,
            input: "in".into(),
        }
    }
}
