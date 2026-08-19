//! One delegated task.
//!
//! Wraps a subagent: reacts to its turn lifecycle, owes its parent every
//! terminal result, and answers how it runs — from the settings snapshotted
//! into its own record, never a resolution walk.

use crate::sessions::spec::SessionSpec;

use super::super::types::TurnEnd;
use super::RunnerBehavior;
use super::action::{OutcomeDecision, Repair, RunnerAction};
use super::deliver;
use super::event::{RecordedEnd, SessionEvent};
use super::ids::{AgentId, RunnerId};
use super::role::{AgentRole, SUBAGENT_PROMPT_SUFFIX, StopHookKind, TitleScope};
use super::state::{RunnerState, SessionState, SubPhase, SubState};

pub(crate) struct SubAgentRunner {
    /// The runner's id — and, same uuid, its agent's.
    pub id: RunnerId,
}

impl SubAgentRunner {
    fn agent(&self) -> AgentId {
        AgentId(self.id.0)
    }

    fn node<'a>(&self, state: &'a SessionState) -> Option<&'a SubState> {
        match &state.record(self.id)?.state {
            RunnerState::Sub(s) => Some(s),
            RunnerState::Main(_) | RunnerState::Fork(_) | RunnerState::Workflow(_) => None,
        }
    }

    fn is_running(&self, state: &SessionState) -> bool {
        matches!(
            self.node(state).map(|s| &s.phase),
            Some(SubPhase::Running { .. })
        )
    }
}

impl RunnerBehavior for SubAgentRunner {
    fn on_outcome(
        &self,
        _state: &SessionState,
        _agent: AgentId,
        end: TurnEnd,
        now_ms: u64,
    ) -> OutcomeDecision {
        let end = match end {
            TurnEnd::Concluded { output } => RecordedEnd::Concluded { output },
            // `terminal` is not this runner's to act on: the sandbox dying
            // reaches the session through the conversation that owns it, and a
            // parent hears every failure the same way.
            TurnEnd::Failed { error, .. } => RecordedEnd::Failed { error },
            // Defensive: a subagent has no ask or timer tools, so neither
            // outcome should ever occur.
            TurnEnd::Asked => RecordedEnd::Failed {
                error: "subagent asked the user; not supported".to_string(),
            },
            TurnEnd::Parked => RecordedEnd::Failed {
                error: "subagent parked; timers are not supported in sessions".to_string(),
            },
            // A subagent's interruption is repaired from the session's own
            // state at load, which is also where the parent is owed the
            // failure. This report cannot arrive first: a subagent actor stays
            // cold and spawns on demand, so its own recovery runs long after
            // the node was reconciled, and acting on it would fail the same
            // node a second time.
            TurnEnd::Interrupted => return OutcomeDecision::none(),
        };
        OutcomeDecision::advance(vec![SessionEvent::TurnEnded {
            at_ms: now_ms,
            agent: self.agent(),
            end,
        }])
    }

    /// The report this runner owes its parent, if one is unsent.
    fn actions(&self, state: &SessionState) -> Vec<RunnerAction> {
        let Some(record) = state.record(self.id) else {
            return Vec::new();
        };
        let Some(to) = record.parent else {
            return Vec::new();
        };
        self.node(state)
            .and_then(|sub| deliver::sub_part_for(self.id, sub))
            .map(|part| RunnerAction::Deliver {
                to,
                child: self.id,
                part,
            })
            .into_iter()
            .collect()
    }

    /// A node left mid-run by a dead process. Its run is over; the parent is
    /// owed the failure like any other terminal result.
    fn repairs(&self, state: &SessionState) -> Vec<Repair> {
        if self.is_running(state) {
            vec![Repair::FailInterruptedSub { id: self.id }]
        } else {
            Vec::new()
        }
    }

    /// This is the invariant that keeps a forty-minute tool call from being
    /// unloaded out from under itself.
    fn busy(&self, state: &SessionState) -> bool {
        self.is_running(state)
    }

    /// The parent is blocked on this child's result, so stopping it quietly
    /// would leave it waiting for one that can never come. The same shape
    /// recovery delivers for a child a crash left running: the parent hears a
    /// failure, and carries on.
    fn stop_event(
        &self,
        state: &SessionState,
        _agent: AgentId,
        now_ms: u64,
    ) -> Option<SessionEvent> {
        self.is_running(state).then(|| SessionEvent::TurnEnded {
            at_ms: now_ms,
            agent: self.agent(),
            end: RecordedEnd::Failed {
                error: super::STOPPED_ERROR.to_string(),
            },
        })
    }

    fn role(
        &self,
        _spec: &SessionSpec,
        state: &SessionState,
        _agent: AgentId,
    ) -> Option<AgentRole> {
        let node = self.node(state)?;
        Some(AgentRole {
            agent: self.agent(),
            name: self.id.to_string(),
            journal: self.id.0,
            settings: node.settings.clone(),
            prompt_suffix: Some(SUBAGENT_PROMPT_SUFFIX),
            broadcasts: false,
            scoped: Some(self.id.0),
            control_plane: false,
            may_ask: false,
            titles: TitleScope::None,
            step_result: None,
            stop_hook: StopHookKind::SubagentStop,
            agent_type: node.agent_type.clone(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::sessions::session_actor::testing::actor_spec_fixture;

    fn runner_and_state() -> (SubAgentRunner, SessionState, AgentId) {
        let main = agent();
        let sub = agent();
        let state = fold(&[main_created(main), sub_created(sub, main, 100)]);
        (
            SubAgentRunner {
                id: RunnerId::of_agent(sub),
            },
            state,
            sub,
        )
    }

    /// A subagent has no ask or timer tools, so both ends are failures its
    /// parent must hear about — not states to wait out.
    #[test]
    fn an_ask_and_a_park_become_failures() {
        let (r, state, sub) = runner_and_state();
        for (end, expected) in [
            (TurnEnd::Asked, "asked the user"),
            (TurnEnd::Parked, "parked"),
        ] {
            let d = r.on_outcome(&state, sub, end, 10);
            assert!(d.advance);
            let [
                SessionEvent::TurnEnded {
                    end: RecordedEnd::Failed { error },
                    ..
                },
            ] = d.events.as_slice()
            else {
                panic!("expected a failure, got {:?}", d.events);
            };
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn an_interruption_journals_nothing_it_is_repaired_at_load() {
        let (r, state, sub) = runner_and_state();
        let d = r.on_outcome(&state, sub, TurnEnd::Interrupted, 10);
        assert!(d.events.is_empty());
        assert_eq!(
            r.repairs(&state),
            vec![Repair::FailInterruptedSub { id: r.id }]
        );
    }

    #[test]
    fn stopping_a_running_sub_fails_it_with_the_stop_wording() {
        let (r, mut state, sub) = runner_and_state();
        assert!(r.busy(&state));
        let event = r.stop_event(&state, sub, 10).unwrap();
        let SessionEvent::TurnEnded {
            end: RecordedEnd::Failed { error },
            ..
        } = event
        else {
            panic!("expected a failure");
        };
        assert_eq!(error, super::super::STOPPED_ERROR);
        state.apply(&ended(
            sub,
            RecordedEnd::Concluded {
                output: "done".into(),
            },
            20,
        ));
        assert!(!r.busy(&state));
        assert!(r.stop_event(&state, sub, 30).is_none());
        assert!(r.repairs(&state).is_empty());
    }

    #[test]
    fn the_role_speaks_the_snapshot_in_its_own_record() {
        let (r, state, sub) = runner_and_state();
        let role = r.role(&actor_spec_fixture(), &state, sub).unwrap();
        assert_eq!(role.name, sub.to_string());
        assert_eq!(role.journal, sub.0);
        assert!(!role.may_ask);
        assert!(!role.broadcasts);
        assert_eq!(role.titles, TitleScope::None);
        assert_eq!(role.scoped, Some(sub.0));
        assert!(matches!(role.stop_hook, StopHookKind::SubagentStop));
        assert_eq!(role.settings.model, "mock");
    }

    #[test]
    fn a_concluded_sub_owes_its_delivery_through_actions() {
        let (r, mut state, sub) = runner_and_state();
        assert!(
            r.actions(&state).is_empty(),
            "a running sub owes nothing yet"
        );
        state.apply(&ended(
            sub,
            RecordedEnd::Concluded {
                output: "done".into(),
            },
            20,
        ));
        let actions = r.actions(&state);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], RunnerAction::Deliver { .. }));
    }
}
