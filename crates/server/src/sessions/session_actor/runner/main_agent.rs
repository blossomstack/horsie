//! The session's own conversation.
//!
//! Wraps the main agent: reacts to its turn lifecycle, and answers how it
//! runs. It starts nothing at a boundary — a conversation's turns are the
//! agent's own decision, taken against the queue it holds — and owes nobody a
//! result.

use crate::sessions::spec::SessionSpec;

use super::super::types::TurnEnd;
use super::RunnerBehavior;
use super::action::{OutcomeDecision, Repair, RunnerAction};
use super::event::{RecordedEnd, SessionEvent};
use super::ids::{AgentId, RunnerId};
use super::role::{AgentRole, StopHookKind, TitleScope, UNATTENDED_PROMPT_SUFFIX};
use super::state::{RunnerState, SessionState, TurnPhase};

pub(crate) struct MainAgentRunner {
    /// The runner's id — and, same uuid, its agent's and the session's.
    pub id: RunnerId,
}

impl MainAgentRunner {
    fn agent(&self) -> AgentId {
        AgentId(self.id.0)
    }

    fn turn<'a>(&self, state: &'a SessionState) -> Option<&'a TurnPhase> {
        match &state.record(self.id)?.state {
            RunnerState::Main(m) => Some(&m.turn),
            RunnerState::Sub(_) | RunnerState::Fork(_) | RunnerState::Workflow(_) => None,
        }
    }
}

impl RunnerBehavior for MainAgentRunner {
    fn on_outcome(
        &self,
        state: &SessionState,
        _agent: AgentId,
        end: TurnEnd,
        now_ms: u64,
    ) -> OutcomeDecision {
        let agent = self.agent();
        let end = match end {
            TurnEnd::Concluded { .. } => {
                // The output stays in the agent's own journal; the session
                // records only the boundary.
                RecordedEnd::Concluded {
                    output: serde_json::Value::Null,
                }
            }
            TurnEnd::Asked => RecordedEnd::Asked,
            // Only from a session that still believes the turn is running. A
            // turn that failed before the loop began never banked a boundary
            // in the agent's journal, so the agent still calls it open while
            // this runner, which was told directly, has already recorded the
            // failure. The runner owns the merged phase, so the runner
            // decides; a report about anything but a live turn is history
            // already written.
            TurnEnd::Interrupted => {
                if !matches!(self.turn(state), Some(TurnPhase::Running)) {
                    return OutcomeDecision::none();
                }
                RecordedEnd::Interrupted
            }
            // A runtime that a live vendor cannot produce is the one terminal
            // failure: re-provisioning would silently rebuild a workspace the
            // user believes they still have. Everything else is a failed turn
            // they can retry.
            TurnEnd::Failed {
                error,
                terminal: true,
            } => {
                return OutcomeDecision::advance(vec![SessionEvent::SessionFailed {
                    at_ms: now_ms,
                    reason: error,
                }]);
            }
            TurnEnd::Failed {
                error,
                terminal: false,
            } => RecordedEnd::Failed { error },
            TurnEnd::Parked => RecordedEnd::Failed {
                error: "agent parked; timers are not supported in sessions".to_string(),
            },
        };
        OutcomeDecision::advance(vec![SessionEvent::TurnEnded {
            at_ms: now_ms,
            agent,
            end,
        }])
    }

    /// Nothing. The agent holds its own queue and decides when it becomes a
    /// turn — the session has neither the message nor the gate.
    fn actions(&self, _state: &SessionState) -> Vec<RunnerAction> {
        Vec::new()
    }

    /// Nothing. A turn the process died inside is reported by the agent whose
    /// turn it was, from its own recovery, and arrives as an ordinary
    /// interrupted outcome.
    fn repairs(&self, _state: &SessionState) -> Vec<Repair> {
        Vec::new()
    }

    fn busy(&self, state: &SessionState) -> bool {
        matches!(self.turn(state), Some(TurnPhase::Running))
    }

    /// Stop is a turn boundary like any other: the agent drains whatever
    /// arrived while the cancelled turn ran, because a stop cancels the turn,
    /// not the promise.
    fn stop_event(
        &self,
        state: &SessionState,
        _agent: AgentId,
        now_ms: u64,
    ) -> Option<SessionEvent> {
        matches!(self.turn(state), Some(TurnPhase::Running)).then(|| SessionEvent::TurnEnded {
            at_ms: now_ms,
            agent: self.agent(),
            end: RecordedEnd::Stopped,
        })
    }

    fn role(
        &self,
        spec: &SessionSpec,
        _state: &SessionState,
        _agent: AgentId,
    ) -> Option<AgentRole> {
        let settings = spec.agent_settings()?.clone();
        let unattended = spec.is_unattended();
        Some(AgentRole {
            agent: self.agent(),
            name: super::super::MAIN_AGENT_ID.to_string(),
            journal: self.id.0,
            control_plane: settings.control_plane == Some(true),
            settings,
            // An unattended session is told why nobody will answer it; an
            // attended main agent needs no explanation of what it is.
            prompt_suffix: unattended.then_some(UNATTENDED_PROMPT_SUFFIX),
            broadcasts: true,
            scoped: None,
            may_ask: !unattended,
            titles: TitleScope::Session,
            step_result: None,
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
    use super::super::testkit::*;
    use super::*;
    use crate::sessions::session_actor::testing::actor_spec_fixture;

    fn runner_and_state() -> (MainAgentRunner, SessionState, AgentId) {
        let main = agent();
        let state = fold(&[main_created(main)]);
        (
            MainAgentRunner {
                id: super::super::ids::RunnerId::of_agent(main),
            },
            state,
            main,
        )
    }

    #[test]
    fn a_conclusion_journals_a_boundary_without_the_output() {
        let (r, state, main) = runner_and_state();
        let d = r.on_outcome(
            &state,
            main,
            TurnEnd::Concluded {
                output: serde_json::json!("the whole answer"),
            },
            10,
        );
        assert!(d.advance, "a boundary drains owed deliveries");
        let [
            SessionEvent::TurnEnded {
                end: RecordedEnd::Concluded { output },
                ..
            },
        ] = d.events.as_slice()
        else {
            panic!("expected one TurnEnded, got {:?}", d.events);
        };
        assert_eq!(
            output,
            &serde_json::Value::Null,
            "the output stays in the agent's own journal"
        );
    }

    #[test]
    fn a_terminal_failure_fails_the_session_a_retryable_one_only_the_turn() {
        let (r, state, main) = runner_and_state();
        let d = r.on_outcome(
            &state,
            main,
            TurnEnd::Failed {
                error: "gone".into(),
                terminal: true,
            },
            10,
        );
        assert!(matches!(
            d.events.as_slice(),
            [SessionEvent::SessionFailed { .. }]
        ));
        let d = r.on_outcome(
            &state,
            main,
            TurnEnd::Failed {
                error: "500".into(),
                terminal: false,
            },
            10,
        );
        assert!(matches!(
            d.events.as_slice(),
            [SessionEvent::TurnEnded {
                end: RecordedEnd::Failed { .. },
                ..
            }]
        ));
    }

    /// A report about anything but a live turn is history already written.
    #[test]
    fn an_interruption_is_recorded_only_while_the_turn_still_runs() {
        let (r, mut state, main) = runner_and_state();
        let d = r.on_outcome(&state, main, TurnEnd::Interrupted, 10);
        assert!(d.events.is_empty(), "no live turn, nothing to record");
        state.apply(&SessionEvent::TurnBegan {
            at_ms: 5,
            agent: main,
        });
        let d = r.on_outcome(&state, main, TurnEnd::Interrupted, 10);
        assert!(matches!(
            d.events.as_slice(),
            [SessionEvent::TurnEnded {
                end: RecordedEnd::Interrupted,
                ..
            }]
        ));
    }

    #[test]
    fn a_stop_is_a_boundary_only_for_a_running_turn() {
        let (r, mut state, main) = runner_and_state();
        assert!(r.stop_event(&state, main, 10).is_none());
        state.apply(&SessionEvent::TurnBegan {
            at_ms: 5,
            agent: main,
        });
        assert!(r.busy(&state));
        let event = r.stop_event(&state, main, 10).unwrap();
        assert!(matches!(
            event,
            SessionEvent::TurnEnded {
                end: RecordedEnd::Stopped,
                ..
            }
        ));
    }

    #[test]
    fn the_role_is_the_sessions_own() {
        let (r, state, main) = runner_and_state();
        let spec = actor_spec_fixture();
        let role = r.role(&spec, &state, main).unwrap();
        assert_eq!(role.name, "main");
        assert_eq!(role.journal, r.id.0);
        assert!(role.may_ask);
        assert_eq!(role.titles, TitleScope::Session);
        assert!(role.prompt_suffix.is_none());
        assert!(role.scoped.is_none());
        assert!(role.broadcasts);
        assert!(!role.requires_result());
    }
}
