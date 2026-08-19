//! One fork of a conversation.
//!
//! Wraps a fork's agent. A fork is a conversation, not delegated work: it
//! owes nobody a result, it can ask the user, and it names itself. What sets
//! it apart from the main conversation is its birth — seeded from another
//! agent's history at a branch point — and that its failures are its own,
//! except the one failure (a dead runtime) every conversation in the session
//! shares.

use crate::sessions::spec::SessionSpec;

use super::super::types::TurnEnd;
use super::RunnerBehavior;
use super::action::{OutcomeDecision, Repair, RunnerAction};
use super::event::{RecordedEnd, SessionEvent};
use super::ids::{AgentId, RunnerId};
use super::role::{AgentRole, FORK_PROMPT_SUFFIX, StopHookKind, TitleScope};
use super::state::{ForkState, RunnerState, SeedPhase, SessionState, TurnPhase};

pub(crate) struct ForkAgentRunner {
    /// The runner's id — and, same uuid, its agent's.
    pub id: RunnerId,
}

impl ForkAgentRunner {
    fn agent(&self) -> AgentId {
        AgentId(self.id.0)
    }

    fn fork<'a>(&self, state: &'a SessionState) -> Option<&'a ForkState> {
        match &state.record(self.id)?.state {
            RunnerState::Fork(f) => Some(f),
            RunnerState::Main(_) | RunnerState::Sub(_) | RunnerState::Workflow(_) => None,
        }
    }
}

impl RunnerBehavior for ForkAgentRunner {
    fn on_outcome(
        &self,
        state: &SessionState,
        _agent: AgentId,
        end: TurnEnd,
        now_ms: u64,
    ) -> OutcomeDecision {
        let end = match end {
            TurnEnd::Concluded { .. } => RecordedEnd::Concluded {
                output: serde_json::Value::Null,
            },
            TurnEnd::Asked => RecordedEnd::Asked,
            // Session-wide, exactly as it is for the main agent: forks share
            // the one runtime, so a runtime that cannot be rebuilt takes every
            // conversation in the session with it, not just this one.
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
            // Only from a fork this runner still believes is running — the
            // same guard, for the same reason, as the main agent's.
            TurnEnd::Interrupted => {
                let running = self
                    .fork(state)
                    .is_some_and(|f| matches!(f.turn, TurnPhase::Running));
                if !running {
                    return OutcomeDecision::none();
                }
                RecordedEnd::Interrupted
            }
        };
        OutcomeDecision::advance(vec![SessionEvent::TurnEnded {
            at_ms: now_ms,
            agent: self.agent(),
            end,
        }])
    }

    /// Nothing: a fork's turns are its agent's own decision, and it owes
    /// nobody a result.
    fn actions(&self, _state: &SessionState) -> Vec<RunnerAction> {
        Vec::new()
    }

    /// A fork whose seed never landed. Nothing else can finish one: seeding is
    /// session-owned work with no journal of its own, so — unlike a turn,
    /// which the agent reports as interrupted from its own recovery — only
    /// this repair brings it back. Safe to re-attempt: `Seeding` is precisely
    /// the state in which no turn has run.
    fn repairs(&self, state: &SessionState) -> Vec<Repair> {
        if self
            .fork(state)
            .is_some_and(|f| matches!(f.seed, SeedPhase::Seeding))
        {
            vec![Repair::ReseedFork { id: self.id }]
        } else {
            Vec::new()
        }
    }

    /// Mid-seed, a summariser call is provider time with nothing durable
    /// behind it — unloading loses it. Mid-turn, unloading cancels a
    /// conversation somebody is having.
    fn busy(&self, state: &SessionState) -> bool {
        self.fork(state).is_some_and(|f| {
            matches!(f.seed, SeedPhase::Seeding) || matches!(f.turn, TurnPhase::Running)
        })
    }

    fn stop_event(
        &self,
        state: &SessionState,
        _agent: AgentId,
        now_ms: u64,
    ) -> Option<SessionEvent> {
        self.fork(state)
            .is_some_and(|f| matches!(f.turn, TurnPhase::Running))
            .then(|| SessionEvent::TurnEnded {
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
        // A fork runs under the session's settings — it is the session's
        // conversation, branched. A workflow session has none, which is one of
        // the two reasons a run cannot be forked.
        let settings = spec.agent_settings()?.clone();
        Some(AgentRole {
            agent: self.agent(),
            name: self.id.to_string(),
            journal: self.id.0,
            settings,
            prompt_suffix: Some(FORK_PROMPT_SUFFIX),
            broadcasts: true,
            scoped: Some(self.id.0),
            control_plane: false,
            may_ask: !spec.is_unattended(),
            titles: TitleScope::Fork(self.id.0),
            step_result: None,
            stop_hook: StopHookKind::Stop,
            agent_type: None,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::event::RunnerEvent;
    use super::super::testkit::*;
    use super::*;
    use crate::sessions::session_actor::testing::actor_spec_fixture;

    fn runner_and_state() -> (ForkAgentRunner, SessionState, AgentId) {
        let main = agent();
        let fork = agent();
        let mut state = fold(&[main_created(main), fork_created(fork, main, 100)]);
        state.apply(&SessionEvent::Runner {
            id: RunnerId::of_agent(fork),
            at_ms: 150,
            event: RunnerEvent::ForkSeeded,
        });
        (
            ForkAgentRunner {
                id: RunnerId::of_agent(fork),
            },
            state,
            fork,
        )
    }

    /// Forks share the one runtime, so a runtime that cannot be rebuilt takes
    /// every conversation in the session with it.
    #[test]
    fn a_terminal_failure_is_session_wide_a_retryable_one_is_the_forks_own() {
        let (r, state, fork) = runner_and_state();
        let d = r.on_outcome(
            &state,
            fork,
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
            fork,
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

    #[test]
    fn an_interruption_is_recorded_only_while_the_fork_still_runs() {
        let (r, mut state, fork) = runner_and_state();
        assert!(
            r.on_outcome(&state, fork, TurnEnd::Interrupted, 10)
                .events
                .is_empty()
        );
        state.apply(&SessionEvent::TurnBegan {
            at_ms: 5,
            agent: fork,
        });
        assert!(
            !r.on_outcome(&state, fork, TurnEnd::Interrupted, 10)
                .events
                .is_empty()
        );
    }

    /// Mid-seed there is provider time with nothing durable behind it;
    /// mid-turn there is a conversation somebody is having. Both refuse an
    /// offload — the seeding half is the gap the old busy list left open.
    #[test]
    fn seeding_and_running_both_refuse_an_offload() {
        let main = agent();
        let fork = agent();
        let state = fold(&[main_created(main), fork_created(fork, main, 100)]);
        let r = ForkAgentRunner {
            id: RunnerId::of_agent(fork),
        };
        assert!(r.busy(&state), "seeding refuses an offload");
        assert_eq!(r.repairs(&state), vec![Repair::ReseedFork { id: r.id }]);
        let (r, mut state, fork) = runner_and_state();
        assert!(!r.busy(&state), "seeded and idle is unloadable");
        assert!(r.repairs(&state).is_empty());
        state.apply(&SessionEvent::TurnBegan {
            at_ms: 5,
            agent: fork,
        });
        assert!(r.busy(&state));
    }

    #[test]
    fn the_role_is_a_conversations_with_its_own_title() {
        let (r, state, fork) = runner_and_state();
        let role = r.role(&actor_spec_fixture(), &state, fork).unwrap();
        assert!(role.may_ask);
        assert_eq!(role.titles, TitleScope::Fork(fork.0));
        assert_eq!(role.prompt_suffix, Some(FORK_PROMPT_SUFFIX));
        assert_eq!(role.scoped, Some(fork.0));
        assert!(role.broadcasts);
    }
}
