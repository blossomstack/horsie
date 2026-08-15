//! The sandbox: the one runner that owns no agents.
//!
//! It owns the sandbox's *lifecycle* — provision, narrate, fail, release — and
//! nothing else. Acquisition stays off this path entirely: an agent's
//! `provide()` reaches the runtime manager on the agent's own task, because
//! routing a thirty-second toolbox build through the session mailbox is exactly
//! what that separation was built to avoid, on a per-turn path.
//!
//! **There is no [`super::AgentLifecycle`] impl here, deliberately.** Every
//! other runner has one; this one owns no agents, so "a runner with no agents
//! cannot be handed an agent event" is a fact about the type rather than an
//! unreachable arm somebody has to keep true. [`super::RunnerState::lifecycle`]
//! returns `None` for this arm because it has nothing to return, not because a
//! match decided so.
//!
//! Its whole reason for existing is [`State::ready`]: one gate, read once by
//! the session into `SessionView::runtime_ready`, in front of every other
//! runner's `actions`. Before it, "is the sandbox up" was a question each
//! caller answered for itself.

use super::action::Action;
use super::projection::Description;
use super::{Runner, RunnerEvent, SessionView};
use crate::sessions::spec::SessionStatus;
use serde::{Deserialize, Serialize};

/// Where the sandbox is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Phase {
    /// Recorded, and nothing asked for yet.
    #[default]
    Pending,
    Provisioning,
    Ready,
    /// `terminal` means no later attempt brings this sandbox back — a vendor
    /// that refused rather than one that timed out.
    Failed {
        terminal: bool,
    },
    /// Handed back. The session may still be read; nothing may run in it.
    Released,
}

/// The sandbox as the session records it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// When the sandbox in use came up. Kept across a release: when it came up
    /// stays true afterwards, and [`Phase`] is the only thing that says whether
    /// one exists — so a stale stamp can never read as readiness.
    pub provisioned_at_ms: Option<u64>,
    pub phase: Phase,
    /// The vendor's own words about what is happening, shown to the person.
    /// One line, so a failure's reason replaces the last progress note rather
    /// than sitting in a second field nothing renders.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Started,
    Progress {
        detail: String,
    },
    /// Carries its own timestamp, unlike every other event here: when the
    /// sandbox came up is a fact about the sandbox, not about when the entry
    /// was written.
    Succeeded {
        at_ms: u64,
    },
    Failed {
        error: String,
        terminal: bool,
    },
    Released,
}

impl State {
    /// Whether a sandbox exists to run in. The session's one gate, which it
    /// reads into `SessionView::runtime_ready` for every other runner.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.phase == Phase::Ready
    }
}

impl Runner for State {
    /// The same "a `Pending` runner asks for its first thing" every other
    /// runner is, with a sandbox in place of an agent.
    ///
    /// A non-terminal failure asks again, because that is what non-terminal
    /// means: the vendor had no capacity, or the call timed out, and the next
    /// boundary is exactly when to retry. A terminal one asks for nothing ever
    /// again — retrying a refusal is a loop, not a recovery — and neither does
    /// a sandbox that is coming up, is up, or has been handed back.
    fn actions(&self, _view: &SessionView) -> Vec<Action> {
        match self.phase {
            Phase::Pending | Phase::Failed { terminal: false } => vec![Action::Provision],
            Phase::Provisioning | Phase::Ready | Phase::Failed { terminal: true } => Vec::new(),
            Phase::Released => Vec::new(),
        }
    }

    fn busy(&self) -> bool {
        self.phase == Phase::Provisioning
    }

    // Both capability accessors keep the trait's `None`: capabilities equip
    // agents, and this runner starts none.

    /// No agents, and the one standing that **overrides** the root's.
    ///
    /// Nothing can run without a sandbox, so what the sandbox is doing is the
    /// session's status whatever its conversation last did — a person looking
    /// at a session whose runtime is still coming up must not be told it is
    /// idle, and one whose runtime can never be built must not be told it can
    /// simply try again.
    ///
    /// `None` once it is up, which is the state that lets the root speak. A
    /// released sandbox says nothing either: the session may still be read, and
    /// what it last did is still the truest thing about it.
    fn describe(&self) -> Description<'_> {
        Description {
            standing: match self.phase {
                Phase::Pending | Phase::Provisioning => Some(SessionStatus::Provisioning),
                // The vendor's own words, which is what the person is shown.
                Phase::Failed { terminal: false } => Some(SessionStatus::ProvisioningFailed {
                    reason: self.detail.clone().unwrap_or_default(),
                }),
                Phase::Failed { terminal: true } => Some(SessionStatus::Unrecoverable {
                    reason: self.detail.clone().unwrap_or_default(),
                }),
                Phase::Ready | Phase::Released => None,
            },
            ..Description::default()
        }
    }

    /// `at_ms` is unread here on purpose. [`Event::Succeeded`] carries its own
    /// stamp, and it means something else: when the *sandbox* came up, not when
    /// the entry recording it was written.
    fn apply(&mut self, event: &RunnerEvent, _at_ms: u64) {
        let RunnerEvent::Runtime(event) = event else {
            return;
        };
        match event {
            Event::Started => {
                self.phase = Phase::Provisioning;
                // A fresh attempt clears the last one's words: showing a person
                // the previous failure while a new sandbox is coming up is the
                // one thing this field must never do.
                self.detail = None;
            }
            Event::Progress { detail } => self.detail = Some(detail.clone()),
            Event::Succeeded { at_ms } => {
                self.phase = Phase::Ready;
                self.provisioned_at_ms = Some(*at_ms);
            }
            Event::Failed { error, terminal } => {
                self.phase = Phase::Failed {
                    terminal: *terminal,
                };
                self.detail = Some(error.clone());
            }
            Event::Released => self.phase = Phase::Released,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::runners::RunnerState;

    fn view() -> SessionView {
        SessionView {
            runtime_ready: false,
            depth: 0,
            active_agents: 0,
        }
    }

    fn phased(phase: Phase) -> State {
        State {
            phase,
            ..State::default()
        }
    }

    /// A recorded sandbox nobody has asked for yet asks for itself, and one
    /// already coming up or already up does not — otherwise every boundary
    /// would ask for a second sandbox while the first was still booting.
    #[test]
    fn a_pending_sandbox_asks_to_be_provisioned_and_asks_once() {
        assert!(matches!(
            phased(Phase::Pending).actions(&view()).as_slice(),
            [Action::Provision]
        ));
        for phase in [Phase::Provisioning, Phase::Ready, Phase::Released] {
            assert!(
                phased(phase).actions(&view()).is_empty(),
                "{phase:?} asked for a sandbox it should not have"
            );
        }
    }

    /// **A terminal failure asks for nothing, ever again.** `actions` is called
    /// at every boundary, so a terminal failure that asked again would retry a
    /// refusal in a loop for as long as the session was resident — against a
    /// vendor that has already said no. A non-terminal one is the opposite
    /// case, and the whole reason the flag exists: no capacity or a timeout is
    /// exactly what the next boundary should try again.
    #[test]
    fn a_runtime_that_failed_terminally_asks_for_nothing() {
        assert!(
            phased(Phase::Failed { terminal: true })
                .actions(&view())
                .is_empty()
        );
        assert!(matches!(
            phased(Phase::Failed { terminal: false })
                .actions(&view())
                .as_slice(),
            [Action::Provision]
        ));
    }

    /// The sandbox is not a unit of work, so it never reaches a status. A
    /// runtime that reported `Failed` here would be marked terminal by the
    /// session and stop being retried at all.
    #[test]
    fn the_sandbox_never_finishes() {
        for phase in [
            Phase::Pending,
            Phase::Provisioning,
            Phase::Ready,
            Phase::Failed { terminal: true },
            Phase::Released,
        ] {
            assert!(
                phased(phase).finished().is_none(),
                "{phase:?} reported a terminal status"
            );
        }
    }

    /// The real assertion is the one a test cannot make: there is no
    /// `AgentLifecycle` impl in this file, so this runner *cannot* be handed an
    /// agent event — it is a compile error, not a runtime check. What is left
    /// to assert is the two visible consequences: no capabilities to equip an
    /// agent with, and `lifecycle()` with nothing to return.
    #[test]
    fn a_runner_with_no_agents_has_no_capabilities_and_no_lifecycle() {
        let state = State::default();
        assert!(state.capabilities().is_none());
        assert!(RunnerState::Runtime(state).lifecycle().is_none());
    }

    /// `busy` is what stops the session unloading. A sandbox coming up is work
    /// in flight; one that is up, failed or gone is not.
    #[test]
    fn busy_only_while_provisioning() {
        let mut state = State::default();
        assert!(!state.busy(), "nothing has been asked for yet");
        state.apply(&RunnerEvent::Runtime(Event::Started), 0);
        assert!(state.busy());
        state.apply(&RunnerEvent::Runtime(Event::Succeeded { at_ms: 7 }), 0);
        assert!(!state.busy());
    }

    /// Readiness is the phase and only the phase. Reading it off
    /// `provisioned_at_ms` would call a released sandbox ready, because the
    /// stamp of when it came up stays true after it is gone.
    #[test]
    fn ready_is_the_phase_and_survives_nothing_else() {
        let mut state = State::default();
        assert!(!state.ready());
        state.apply(&RunnerEvent::Runtime(Event::Started), 0);
        assert!(!state.ready());
        state.apply(&RunnerEvent::Runtime(Event::Succeeded { at_ms: 42 }), 0);
        assert!(state.ready());
        assert_eq!(state.provisioned_at_ms, Some(42));

        state.apply(&RunnerEvent::Runtime(Event::Released), 0);
        assert!(!state.ready());
        assert_eq!(
            state.provisioned_at_ms,
            Some(42),
            "when it came up stays true; the phase is what says it is gone"
        );
    }

    /// A failure keeps the vendor's own words and whether trying again could
    /// ever help — the two things a person needs to decide what to do next.
    #[test]
    fn a_failure_keeps_the_vendors_words_and_whether_it_is_final() {
        let mut state = State::default();
        state.apply(
            &RunnerEvent::Runtime(Event::Failed {
                error: "no capacity in ord".into(),
                terminal: false,
            }),
            0,
        );
        assert_eq!(state.phase, Phase::Failed { terminal: false });
        assert_eq!(state.detail.as_deref(), Some("no capacity in ord"));
        assert!(!state.ready());
    }

    /// Progress narrates while the sandbox comes up, and a retry starts from a
    /// blank line rather than showing the previous failure under a fresh
    /// attempt.
    #[test]
    fn a_retry_clears_the_last_failures_words() {
        let mut state = State::default();
        state.apply(
            &RunnerEvent::Runtime(Event::Progress {
                detail: "pulling the image".into(),
            }),
            0,
        );
        assert_eq!(state.detail.as_deref(), Some("pulling the image"));
        state.apply(
            &RunnerEvent::Runtime(Event::Failed {
                error: "vendor refused".into(),
                terminal: true,
            }),
            0,
        );
        state.apply(&RunnerEvent::Runtime(Event::Started), 0);
        assert_eq!(state.phase, Phase::Provisioning);
        assert!(state.detail.is_none());
    }

    /// The slice is snapshotted, so it has to survive a round trip.
    #[test]
    fn the_slice_round_trips_through_the_journal() {
        let mut state = State::default();
        state.apply(&RunnerEvent::Runtime(Event::Succeeded { at_ms: 9 }), 0);
        state.apply(
            &RunnerEvent::Runtime(Event::Progress {
                detail: "warm".into(),
            }),
            0,
        );
        let back: State = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(back.phase, Phase::Ready);
        assert_eq!(back.provisioned_at_ms, Some(9));
        assert_eq!(back.detail.as_deref(), Some("warm"));
    }
}
