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

use crate::sessions::session_actor::{AgentKey, SessionDomainEvent, SessionState};
use crate::sessions::subagents::SubAgentParent;
use horsie_agentcore::{
    AskLifecycle, EmptyOutcome, FailedOutcome, LifecycleEvent, ProvisioningLifecycle,
    QueuedLifecycle, SessionFailedLifecycle, StepLifecycle, SubAgentLifecycle, TurnBeganLifecycle,
    TurnEndedLifecycle, TurnOutcome,
};

/// One entry: whose log it belongs in, and what it says there.
type Entry = (AgentKey, LifecycleEvent);

/// Every log this event belongs in, and what it becomes in each.
///
/// A list rather than one destination, because a fact can matter to more than
/// one reader: a subagent's result is both its own last word and news to the
/// parent that is waiting on it. Bookkeeping returns an empty list, which is how
/// "nothing a viewer would render" is said — a usage total is a number, and
/// `SubAgentRunning`/`SubAgentNotified` are the session reconciling its own tree.
///
/// Takes the state as it stands *after* the event, because two of the routings
/// are not in the event: a step execution knows only its index, and a subagent's
/// terminal result knows only its own id. Both find their agent in the run log
/// and the forest respectively.
#[must_use]
pub fn route(event: &SessionDomainEvent, state: &SessionState) -> Vec<Entry> {
    use SessionDomainEvent as E;
    // A session-wide fact belongs in the log a person reads when they open the
    // session. That is the main agent for a conversation; a run has none, so it
    // goes to the step in flight, whose log is the only one there is.
    let session_wide = |state: &SessionState| -> Option<AgentKey> {
        match state.run.as_ref() {
            None => Some(AgentKey::Main),
            Some(run) => run.current_agent().map(AgentKey::Step),
        }
    };
    let on_session = |state: &SessionState, ev: LifecycleEvent| -> Vec<Entry> {
        session_wide(state)
            .map(|key| (key, ev))
            .into_iter()
            .collect()
    };
    match event {
        E::ProvisioningStarted { .. } => on_session(
            state,
            LifecycleEvent::Provisioning(ProvisioningLifecycle {
                stage: "acquiring_runtime".into(),
                detail: None,
            }),
        ),
        E::ProvisioningSucceeded { .. } => on_session(
            state,
            LifecycleEvent::Provisioning(ProvisioningLifecycle {
                stage: "ready".into(),
                detail: None,
            }),
        ),
        E::ProvisioningFailed { error, .. } => on_session(
            state,
            LifecycleEvent::Provisioning(ProvisioningLifecycle {
                stage: "failed".into(),
                detail: Some(error.clone()),
            }),
        ),
        E::MessageQueued { id, text, .. } => on_session(
            state,
            LifecycleEvent::MessageQueued(QueuedLifecycle {
                id: id.clone(),
                text: text.clone(),
            }),
        ),
        E::TurnBegan {
            consumed, answered, ..
        } => on_session(
            state,
            LifecycleEvent::TurnBegan(TurnBeganLifecycle {
                consumed: consumed.clone(),
                answered: answered.clone(),
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
        E::AskRecorded {
            tool_call_id,
            question,
            ..
        } => on_session(
            state,
            LifecycleEvent::AskRecorded(AskLifecycle {
                tool_call_id: tool_call_id.clone(),
                question: question.clone(),
            }),
        ),
        E::SessionFailed { reason, .. } => on_session(
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
        } => vec![(
            parent_key(parent, state),
            LifecycleEvent::SubAgent(SubAgentLifecycle {
                id: id.to_string(),
                label: label.clone(),
                status: "running".into(),
            }),
        )],
        // On the parent too, and for the same reason the spawn is: the parent is
        // what a person has open while it waits. It used to reach only the
        // child, whose own log already ends with the report.
        E::SubAgentCompleted { id, .. } => terminal_subagent(*id, "completed", state),
        E::SubAgentFailed { id, .. } => terminal_subagent(*id, "failed", state),
        // A step's own log, which for a run is the only one there is. These used
        // to route to `Main`, which a run does not have, so every one of them was
        // dropped with a warning.
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
        E::StepConcluded { index, .. } => step_entry(*index, "concluded", state),
        E::StepFailed { index, .. } => step_entry(*index, "failed", state),
        E::StepCancelled { index, .. } => step_entry(*index, "cancelled", state),
        // The run's own end, recorded on whichever step last ran — there is no
        // other log to put it in.
        E::RunFinished { .. } => last_step_entry("run_finished", state),
        E::RunFailed { .. } => last_step_entry("run_failed", state),
        // Bookkeeping, deliberately not surfaced. A usage total is a number on
        // the agent document; `SubAgentRunning` and `SubAgentNotified` are the
        // session reconciling its own tree, and a viewer already sees the
        // spawn and the result.
        E::UsageRecorded { .. } | E::SubAgentRunning { .. } | E::SubAgentNotified { .. } => {
            Vec::new()
        }
    }
}

/// A finished subagent, on its parent. The label comes off the forest — the
/// event carries only the id, and a bare uuid is not something a reader can
/// place.
fn terminal_subagent(id: uuid::Uuid, status: &str, state: &SessionState) -> Vec<Entry> {
    let Some(record) = state.subagents.node(id) else {
        return Vec::new();
    };
    vec![(
        parent_key(&record.parent, state),
        LifecycleEvent::SubAgent(SubAgentLifecycle {
            id: id.to_string(),
            label: record.label.clone(),
            status: status.into(),
        }),
    )]
}

/// One step execution's entry, on that step's own agent. The agent and the name
/// both come from the run log, which this event has already been folded into.
fn step_entry(index: u32, status: &str, state: &SessionState) -> Vec<Entry> {
    let Some(step) = state.run.as_ref().and_then(|r| r.get(index)) else {
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

/// The run's own outcome, on the step that ran last.
fn last_step_entry(status: &str, state: &SessionState) -> Vec<Entry> {
    let Some(run) = state.run.as_ref() else {
        return Vec::new();
    };
    let last = run.steps.len().checked_sub(1);
    match last {
        Some(index) => step_entry(index as u32, status, state),
        None => Vec::new(),
    }
}

/// Whose log a subagent's news belongs in: the spawning subagent, or — for a
/// top-level spawn — whatever this session's own "Main" is.
fn parent_key(parent: &SubAgentParent, state: &SessionState) -> AgentKey {
    match parent {
        SubAgentParent::SubAgent(id) => AgentKey::Sub(*id),
        SubAgentParent::Main => match state.run.as_ref().and_then(|r| r.current_agent()) {
            Some(agent) => AgentKey::Step(agent),
            None => AgentKey::Main,
        },
    }
}

#[cfg(test)]
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::session_actor::SessionActor;
    use crate::sessions::subagents::TreeOwner;
    use horsie_actor::EventSourcedActor;
    use uuid::Uuid;

    /// Fold a log the way the actor does, so `route` is asked about the state
    /// its event actually produced rather than a hand-built one.
    fn fold(events: Vec<SessionDomainEvent>) -> SessionState {
        events
            .into_iter()
            .fold(SessionState::default(), SessionActor::apply_event)
    }

    /// Every variant, listed by hand rather than derived.
    ///
    /// That is the point: adding a `SessionDomainEvent` breaks the compilation
    /// of this list until someone decides where it goes. A forgotten routing is
    /// a fact that silently never reaches a client, which no other test would
    /// catch — the session would journal it, fold it, and nobody would ever see
    /// it.
    fn every_variant(sub: Uuid, step_agent: Uuid) -> Vec<SessionDomainEvent> {
        use SessionDomainEvent as E;
        vec![
            E::ProvisioningStarted { at_ms: 1 },
            E::ProvisioningSucceeded { at_ms: 1 },
            E::ProvisioningFailed {
                at_ms: 1,
                error: "no".into(),
                terminal: false,
            },
            E::MessageQueued {
                id: "m1".into(),
                text: "hi".into(),
                at_ms: 1,
            },
            E::TurnBegan {
                at_ms: 1,
                consumed: vec!["m1".into()],
                answering: None,
                answered: vec![],
            },
            E::AskRecorded {
                at_ms: 1,
                tool_call_id: Some("tc".into()),
                question: "which?".into(),
            },
            E::TurnEnded { at_ms: 1 },
            E::TurnFailed {
                at_ms: 1,
                error: "boom".into(),
            },
            E::TurnStopped { at_ms: 1 },
            E::TurnInterrupted { at_ms: 1 },
            E::SessionFailed {
                at_ms: 1,
                reason: "dead".into(),
            },
            E::UsageRecorded {
                at_ms: 1,
                agent_id: "main".into(),
                usage_total: horsie_workflow::UsageTotal::default(),
            },
            E::SubAgentSpawned {
                at_ms: 1,
                id: sub,
                parent: SubAgentParent::Main,
                label: "l".into(),
                task: "t".into(),
                depth: 1,
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
            E::StepStarted {
                at_ms: 1,
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
                index: 0,
                output: serde_json::Value::Null,
            },
            E::StepFailed {
                at_ms: 1,
                index: 0,
                error: "no".into(),
            },
            E::StepCancelled { at_ms: 1, index: 0 },
            E::RunFinished {
                at_ms: 1,
                output: serde_json::Value::Null,
            },
            E::RunFailed {
                at_ms: 1,
                error: "no".into(),
            },
        ]
    }

    /// Whether this event is part of a workflow run, and so has to be routed
    /// against a run's state rather than a conversation's.
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
    /// Each event is asked against the state it would really occur in — a run's
    /// step events against a run mid-step, everything else against a
    /// conversation — because two of the routings resolve their agent from
    /// state rather than from the event.
    #[test]
    fn every_viewer_facing_event_has_a_destination() {
        let sub = Uuid::new_v4();
        let step_agent = Uuid::new_v4();
        let conversation = fold(vec![SessionDomainEvent::SubAgentSpawned {
            at_ms: 1,
            id: sub,
            parent: SubAgentParent::Main,
            label: "l".into(),
            task: "t".into(),
            depth: 1,
            agent_type: None,
        }]);
        let run = fold(vec![step_context_for(step_agent)]);
        for event in every_variant(sub, step_agent) {
            let bookkeeping = matches!(
                event,
                SessionDomainEvent::UsageRecorded { .. }
                    | SessionDomainEvent::SubAgentRunning { .. }
                    | SessionDomainEvent::SubAgentNotified { .. }
            );
            let state = match is_run_event(&event) {
                true => &run,
                false => &conversation,
            };
            let entries = route(&event, state);
            match bookkeeping {
                true => assert!(entries.is_empty(), "{event:?} is bookkeeping"),
                false => assert!(!entries.is_empty(), "{event:?} has no destination"),
            }
        }
    }

    /// A run that has not started its first step has no log at all yet, so a
    /// session-wide fact in that window is dropped rather than misfiled. This is
    /// the one case with genuinely nowhere to go: step agents are spawned per
    /// step, and a run has no main agent to fall back on.
    #[test]
    fn a_run_with_no_step_yet_has_nowhere_to_record() {
        let state = SessionState {
            run: Some(crate::sessions::workflow::WorkflowRunState::default()),
            ..Default::default()
        };
        assert!(
            route(
                &SessionDomainEvent::ProvisioningStarted { at_ms: 1 },
                &state
            )
            .is_empty()
        );
    }

    /// A spawn and a terminal result both land on the *parent*: that is the log
    /// a person has open while a child is working. The child's own log already
    /// ends with its report.
    #[test]
    fn a_subagents_news_is_recorded_on_its_parent() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let spawn = SessionDomainEvent::SubAgentSpawned {
            at_ms: 1,
            id: child,
            parent: SubAgentParent::SubAgent(parent),
            label: "child".into(),
            task: "t".into(),
            depth: 2,
            agent_type: None,
        };
        let done = SessionDomainEvent::SubAgentCompleted {
            at_ms: 2,
            id: child,
            output: "done".into(),
        };
        let state = fold(vec![spawn.clone(), done.clone()]);
        for event in [spawn, done] {
            let entries = route(&event, &state);
            assert_eq!(entries.len(), 1, "{event:?} routes once");
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
        let agent = Uuid::new_v4();
        let started = SessionDomainEvent::StepStarted {
            at_ms: 1,
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
            index: 0,
            output: serde_json::Value::Null,
        };
        let state = fold(vec![started.clone(), concluded.clone()]);
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

    /// A session-wide fact in a run has nowhere else to go either, so it lands
    /// on the step in flight rather than on a main agent that does not exist.
    #[test]
    fn a_session_wide_fact_in_a_run_lands_on_the_step_in_flight() {
        let agent = Uuid::new_v4();
        let state = fold(vec![step_context_for(agent)]);
        let entries = route(&SessionDomainEvent::TurnEnded { at_ms: 3 }, &state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentKey::Step(agent));
    }

    /// The same fact in a conversation goes to the main agent.
    #[test]
    fn a_session_wide_fact_in_a_conversation_lands_on_main() {
        let state = SessionState::default();
        let entries = route(&SessionDomainEvent::TurnEnded { at_ms: 3 }, &state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentKey::Main);
    }

    /// A spawn under a run's step roots that step's tree, so its news goes to
    /// the step and not to a main agent the run does not have.
    #[test]
    fn a_spawn_under_a_step_is_recorded_on_the_step() {
        let agent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let spawn = SessionDomainEvent::SubAgentSpawned {
            at_ms: 2,
            id: child,
            parent: SubAgentParent::Main,
            label: "helper".into(),
            task: "t".into(),
            depth: 1,
            agent_type: None,
        };
        let state = fold(vec![step_context_for(agent), spawn.clone()]);
        assert_eq!(
            state.subagents.owner_of(child),
            Some(TreeOwner::Step(agent))
        );
        let entries = route(&spawn, &state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, AgentKey::Step(agent));
    }

    /// Four session events collapse into one lifecycle entry carrying an
    /// outcome, so a consumer asking "is the turn over" does not have to
    /// enumerate the ways it can be.
    #[test]
    fn every_way_a_turn_can_end_becomes_one_entry_kind() {
        let state = SessionState::default();
        let outcomes: Vec<TurnOutcome> = [
            SessionDomainEvent::TurnEnded { at_ms: 1 },
            SessionDomainEvent::TurnFailed {
                at_ms: 1,
                error: "boom".into(),
            },
            SessionDomainEvent::TurnStopped { at_ms: 1 },
            SessionDomainEvent::TurnInterrupted { at_ms: 1 },
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

    fn step_context_for(agent: Uuid) -> SessionDomainEvent {
        SessionDomainEvent::StepStarted {
            at_ms: 1,
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
