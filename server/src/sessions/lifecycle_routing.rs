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

use crate::sessions::session_actor::{AgentKey, SessionDomainEvent};
use crate::sessions::subagents::SubAgentParent;
use horsie_agentcore::{
    AskLifecycle, EmptyOutcome, FailedOutcome, LifecycleEvent, ProvisioningLifecycle,
    QueuedLifecycle, SessionFailedLifecycle, StepLifecycle, SubAgentLifecycle, TurnBeganLifecycle,
    TurnEndedLifecycle, TurnOutcome,
};

/// Whose log an event belongs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleTarget {
    /// The session's primary agent. Session-wide facts land here because it is
    /// the log a person is reading when they open the session.
    Main,
    /// A specific agent — the one the event is about, or the one that needs to
    /// know. A spawned subagent is recorded on its *parent*, because the parent
    /// is what a viewer has open when a child appears beneath it.
    Agent(AgentKey),
    /// Bookkeeping. Nothing a viewer would render, so nothing is appended: a
    /// usage total is a number, and a subagent's internal transitions are the
    /// session's own accounting.
    None,
}

/// Where this event goes, and what it becomes there.
///
/// Returning both together is deliberate — a destination with no payload, or a
/// payload with no destination, is a bug the type can rule out.
#[must_use]
pub fn route(event: &SessionDomainEvent) -> (LifecycleTarget, Option<LifecycleEvent>) {
    use SessionDomainEvent as E;
    match event {
        E::ProvisioningStarted { .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Provisioning(ProvisioningLifecycle {
                stage: "acquiring_runtime".into(),
                detail: None,
            })),
        ),
        E::ProvisioningSucceeded { .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Provisioning(ProvisioningLifecycle {
                stage: "ready".into(),
                detail: None,
            })),
        ),
        E::ProvisioningFailed { error, .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Provisioning(ProvisioningLifecycle {
                stage: "failed".into(),
                detail: Some(error.clone()),
            })),
        ),
        E::MessageQueued { id, text, .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::MessageQueued(QueuedLifecycle {
                id: id.clone(),
                text: text.clone(),
            })),
        ),
        E::TurnBegan {
            consumed, answered, ..
        } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::TurnBegan(TurnBeganLifecycle {
                consumed: consumed.clone(),
                answered: answered.clone(),
            })),
        ),
        E::TurnEnded { .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: TurnOutcome::Ended(EmptyOutcome {}),
            })),
        ),
        E::TurnFailed { error, .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: TurnOutcome::Failed(FailedOutcome {
                    error: error.clone(),
                }),
            })),
        ),
        E::TurnStopped { .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: TurnOutcome::Stopped(EmptyOutcome {}),
            })),
        ),
        E::TurnInterrupted { .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::TurnEnded(TurnEndedLifecycle {
                outcome: TurnOutcome::Interrupted(EmptyOutcome {}),
            })),
        ),
        E::AskRecorded {
            tool_call_id,
            question,
            ..
        } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::AskRecorded(AskLifecycle {
                tool_call_id: tool_call_id.clone(),
                question: question.clone(),
            })),
        ),
        E::SessionFailed { reason, .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::SessionFailed(SessionFailedLifecycle {
                reason: reason.clone(),
            })),
        ),
        // On the parent, not the child: a subagent appearing is something that
        // happens *in* the parent's trajectory, and the child's own log starts
        // with its own work.
        E::SubAgentSpawned {
            id, parent, label, ..
        } => (
            parent_target(parent),
            Some(LifecycleEvent::SubAgent(SubAgentLifecycle {
                id: id.to_string(),
                label: label.clone(),
                status: "running".into(),
            })),
        ),
        E::SubAgentCompleted { id, .. } => (
            LifecycleTarget::Agent(AgentKey::Sub(*id)),
            Some(LifecycleEvent::SubAgent(SubAgentLifecycle {
                id: id.to_string(),
                label: String::new(),
                status: "completed".into(),
            })),
        ),
        E::SubAgentFailed { id, .. } => (
            LifecycleTarget::Agent(AgentKey::Sub(*id)),
            Some(LifecycleEvent::SubAgent(SubAgentLifecycle {
                id: id.to_string(),
                label: String::new(),
                status: "failed".into(),
            })),
        ),
        E::StepStarted { index, .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Step(StepLifecycle {
                index: *index,
                status: "started".into(),
            })),
        ),
        E::StepConcluded { index, .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Step(StepLifecycle {
                index: *index,
                status: "concluded".into(),
            })),
        ),
        E::StepFailed { index, .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Step(StepLifecycle {
                index: *index,
                status: "failed".into(),
            })),
        ),
        E::StepCancelled { index, .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Step(StepLifecycle {
                index: *index,
                status: "cancelled".into(),
            })),
        ),
        E::RunFinished { .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Step(StepLifecycle {
                index: 0,
                status: "run_finished".into(),
            })),
        ),
        E::RunFailed { .. } => (
            LifecycleTarget::Main,
            Some(LifecycleEvent::Step(StepLifecycle {
                index: 0,
                status: "run_failed".into(),
            })),
        ),
        // Bookkeeping, deliberately not surfaced. A usage total is a number on
        // the agent document; `SubAgentRunning` and `SubAgentNotified` are the
        // session reconciling its own tree, and a viewer already sees the
        // spawn and the result.
        E::UsageRecorded { .. } | E::SubAgentRunning { .. } | E::SubAgentNotified { .. } => {
            (LifecycleTarget::None, None)
        }
    }
}

fn parent_target(parent: &SubAgentParent) -> LifecycleTarget {
    use SubAgentParent as P;
    match parent {
        P::Main => LifecycleTarget::Main,
        P::SubAgent(id) => LifecycleTarget::Agent(AgentKey::Sub(*id)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Every variant, listed by hand rather than derived.
    ///
    /// That is the point: adding a `SessionDomainEvent` breaks the compilation
    /// of this list until someone decides where it goes. A forgotten routing is
    /// a fact that silently never reaches a client, which no other test would
    /// catch — the session would journal it, fold it, and nobody would ever see
    /// it.
    fn every_variant() -> Vec<SessionDomainEvent> {
        use SessionDomainEvent as E;
        let id = Uuid::new_v4();
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
                id,
                parent: SubAgentParent::Main,
                label: "l".into(),
                task: "t".into(),
                depth: 1,
                agent_type: None,
            },
            E::SubAgentRunning { at_ms: 1, id },
            E::SubAgentCompleted {
                at_ms: 1,
                id,
                output: "done".into(),
            },
            E::SubAgentFailed {
                at_ms: 1,
                id,
                error: "no".into(),
            },
            E::SubAgentNotified { at_ms: 1, id },
            E::StepStarted {
                at_ms: 1,
                index: 0,
                step: "s".into(),
                agent: id,
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

    #[test]
    fn every_viewer_facing_event_has_a_destination() {
        for event in every_variant() {
            let (target, payload) = route(&event);
            let bookkeeping = matches!(
                event,
                SessionDomainEvent::UsageRecorded { .. }
                    | SessionDomainEvent::SubAgentRunning { .. }
                    | SessionDomainEvent::SubAgentNotified { .. }
            );
            if bookkeeping {
                assert_eq!(target, LifecycleTarget::None, "{event:?} is bookkeeping");
                assert!(
                    payload.is_none(),
                    "{event:?} routes nowhere but carries a payload"
                );
            } else {
                assert_ne!(
                    target,
                    LifecycleTarget::None,
                    "{event:?} has no destination"
                );
                assert!(payload.is_some(), "{event:?} routes but carries nothing");
            }
        }
    }

    /// The one routing that is not "main": a spawn is recorded on whoever asked
    /// for it, so a viewer reading a subagent sees the grandchildren it spawned
    /// rather than having them all pile up on the session's primary log.
    #[test]
    fn a_spawn_is_recorded_on_its_parent() {
        let parent = Uuid::new_v4();
        let (target, payload) = route(&SessionDomainEvent::SubAgentSpawned {
            at_ms: 1,
            id: Uuid::new_v4(),
            parent: SubAgentParent::SubAgent(parent),
            label: "child".into(),
            task: "t".into(),
            depth: 2,
            agent_type: None,
        });
        assert_eq!(target, LifecycleTarget::Agent(AgentKey::Sub(parent)));
        assert!(matches!(payload, Some(LifecycleEvent::SubAgent(_))));
    }

    /// Four session events collapse into one lifecycle entry carrying an
    /// outcome, so a consumer asking "is the turn over" does not have to
    /// enumerate the ways it can be.
    #[test]
    fn every_way_a_turn_can_end_becomes_one_entry_kind() {
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
        .map(|e| match route(e).1 {
            Some(LifecycleEvent::TurnEnded(t)) => t.outcome,
            other => panic!("expected TurnEnded, got {other:?}"),
        })
        .collect();
        assert!(matches!(outcomes[0], TurnOutcome::Ended(_)));
        assert!(matches!(outcomes[1], TurnOutcome::Failed(_)));
        assert!(matches!(outcomes[2], TurnOutcome::Stopped(_)));
        assert!(matches!(outcomes[3], TurnOutcome::Interrupted(_)));
    }
}
