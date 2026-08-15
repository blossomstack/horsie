//! `/fork` and `/summary-n-fork`: branching a conversation.
//!
//! The one capability with no tool. A fork is asked for by a person typing a
//! built-in, never by a model calling something, so it equips a command and
//! nothing else — advertising a tool for it would offer the model a button it
//! has no business pressing.
//!
//! What it creates is a [`RunnerKind::Conversation`], not a kind of its own: a
//! fork *is* a conversation, one that carries a branch point. That collapses
//! the five fork-shaped events the previous shape needed — created, seeded,
//! titled, status-changed, turn-ended — into the conversation vocabulary every
//! runner already speaks.
//!
//! And it takes no [`super::super::message::ChildOutcome`] at all. A fork owes
//! nobody a result, so it reaches its creator through `Ready`/`Failed` only;
//! there is no `Fork` arm to match, which makes "a fork reports a result"
//! unwritable rather than something a reviewer has to notice. `pending` is
//! therefore only ever a seed in flight — the copy or the summary that has to
//! land before the new conversation can run — and empties when it lands.

use super::{CapEvent, Decision, Handler};
use crate::sessions::forks::ForkMode;
use crate::sessions::runners::action::{Action, AgentSpec, Branch, RunnerArgs};
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::message::{Caller, ChildMsg, Command, Message};
use crate::sessions::spec::AgentSettings;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The built-in that copies the source's log.
pub const FORK_COMMAND: &str = "fork";
/// The built-in that summarises it first.
pub const SUMMARY_COMMAND: &str = "summary-n-fork";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkCapability {
    /// Forks whose seed has not landed yet, and which of my agents branched
    /// them. A fork with a seed in flight exists but cannot run.
    pub pending: BTreeMap<RunnerId, AgentId>,
    /// What a fork inherits. Fixed when the owning runner built this, so two
    /// forks of one conversation cannot end up equipped differently.
    pub settings: AgentSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Created { fork: RunnerId, from: AgentId },
    Seeded { fork: RunnerId },
    SeedFailed { fork: RunnerId, error: String },
}

impl ForkCapability {
    #[must_use]
    pub fn new(settings: AgentSettings) -> Self {
        Self {
            pending: BTreeMap::new(),
            settings,
        }
    }

    fn on_command(&self, caller: Caller, c: &Command) -> Option<Decision> {
        let mode = match c.name.as_str() {
            FORK_COMMAND => ForkMode::Copy,
            SUMMARY_COMMAND => ForkMode::Summary,
            _ => return None,
        };
        let message = c.args.trim();
        // A fork with nothing to do is the one refusal this capability makes:
        // it would branch a whole conversation and then sit idle, and the
        // person would have to notice that themselves.
        if message.is_empty() {
            return Some((
                Vec::new(),
                vec![Action::Reply {
                    text: format!(
                        "/{} needs a message saying what the new conversation should do",
                        c.name
                    ),
                }],
            ));
        }
        let fork = RunnerId::new_v4();
        Some((
            vec![CapEvent::Fork(Event::Created {
                fork,
                from: caller.agent,
            })],
            vec![Action::CreateChild {
                id: fork,
                kind: RunnerKind::Conversation,
                args: RunnerArgs::Conversation {
                    seed: Some(Branch {
                        source: caller.agent,
                        // Zero, not a guess: the branch point is wherever the
                        // source's log actually ends when the session cuts it,
                        // and the session stamps it there. A number chosen
                        // here would be one taken before the cut.
                        source_seq: 0,
                        mode,
                    }),
                    message: message.to_string(),
                    settings: Box::new(self.settings.clone()),
                },
                parent: caller.agent,
            }],
        ))
    }

    fn on_child(&self, m: &ChildMsg) -> Option<Decision> {
        match m {
            // The seed landed; the fork is a conversation like any other, and
            // its own runner starts its agent.
            ChildMsg::Ready { child } => {
                self.pending.get(child)?;
                Some((
                    vec![CapEvent::Fork(Event::Seeded { fork: *child })],
                    Vec::new(),
                ))
            }
            // No delivery: a fork owes nobody a result, so a failed one is
            // recorded and shown as that fork's own status rather than sent
            // back to the conversation it branched from.
            ChildMsg::Failed { child, error } => {
                self.pending.get(child)?;
                Some((
                    vec![CapEvent::Fork(Event::SeedFailed {
                        fork: *child,
                        error: error.clone(),
                    })],
                    Vec::new(),
                ))
            }
            ChildMsg::Outcome { .. } => None,
        }
    }
}

impl Handler for ForkCapability {
    /// Equips nothing.
    ///
    /// No tool layer, because `/fork` is typed rather than called, and no
    /// prompt section either: a paragraph about a command the model cannot use
    /// spends context to tell it about something it will never do.
    fn setup(&self, _spec: &mut AgentSpec) {}

    fn handle(&self, caller: Caller, msg: &Message) -> Option<Decision> {
        match msg {
            Message::Command(c) => self.on_command(caller, c),
            Message::Child(m) => self.on_child(m),
            Message::Tool(_) | Message::Ask(_) => None,
        }
    }

    fn apply(&mut self, event: &CapEvent) {
        let CapEvent::Fork(e) = event else { return };
        match e {
            Event::Created { fork, from } => {
                self.pending.insert(*fork, *from);
            }
            // Both endings clear it: `pending` records a seed in flight, and a
            // seed that failed is no longer in flight either.
            Event::Seeded { fork } | Event::SeedFailed { fork, .. } => {
                self.pending.remove(fork);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::sessions::runners::action::ToolLayer;
    use crate::sessions::runners::message::{ChildOutcome, SubAgentOutcome};

    fn cap() -> ForkCapability {
        ForkCapability::new(settings())
    }

    fn command(name: &str, args: &str) -> Message {
        Message::Command(Command {
            name: name.into(),
            args: args.into(),
        })
    }

    fn fork_via(c: &mut ForkCapability, caller: Caller, name: &str) -> RunnerId {
        let (events, actions) = c
            .handle(caller, &command(name, "look into it"))
            .expect("mine");
        c.apply(&events[0]);
        let Action::CreateChild { id, .. } = &actions[0] else {
            panic!("expected a create, got {:?}", actions[0]);
        };
        *id
    }

    /// A fork is a conversation with a branch point, and the branch names the
    /// agent whose log it was cut from. Getting that wrong is how a fork of a
    /// fork used to read as a fork of something else entirely.
    #[test]
    fn forking_creates_a_conversation_branched_from_the_caller() {
        let c = cap();
        let caller = caller();
        let (events, actions) = c
            .handle(caller, &command(FORK_COMMAND, "  look into the flake  "))
            .expect("mine");
        let CapEvent::Fork(Event::Created { fork, from }) = &events[0] else {
            panic!("expected a create, got {:?}", events[0]);
        };
        assert_eq!(*from, caller.agent);
        let Action::CreateChild {
            id,
            kind,
            args,
            parent,
        } = &actions[0]
        else {
            panic!("expected a create, got {:?}", actions[0]);
        };
        assert_eq!(id, fork);
        assert_eq!(*kind, RunnerKind::Conversation);
        assert_eq!(*parent, caller.agent);
        let RunnerArgs::Conversation { seed, message, .. } = args else {
            panic!("expected conversation args, got {args:?}");
        };
        assert_eq!(message, "look into the flake");
        let seed = seed.as_ref().expect("a fork has a branch point");
        assert_eq!(seed.source, caller.agent);
        assert_eq!(seed.mode, ForkMode::Copy);
    }

    /// The two built-ins differ only in how the new conversation is seeded, so
    /// one handler serves both and the mode is the whole difference.
    #[test]
    fn summary_n_fork_seeds_with_a_summary() {
        let c = cap();
        let (_, actions) = c
            .handle(caller(), &command(SUMMARY_COMMAND, "carry on elsewhere"))
            .expect("mine");
        let Action::CreateChild { args, .. } = &actions[0] else {
            panic!("expected a create, got {:?}", actions[0]);
        };
        let RunnerArgs::Conversation { seed, .. } = args else {
            panic!("expected conversation args, got {args:?}");
        };
        assert_eq!(seed.as_ref().map(|s| s.mode), Some(ForkMode::Summary));
    }

    /// An empty message is refused in words and journals nothing: a fork with
    /// nothing to do would branch a conversation and then sit there.
    #[test]
    fn an_empty_message_is_refused_and_journals_nothing() {
        let c = cap();
        let (events, actions) = c
            .handle(caller(), &command(FORK_COMMAND, "   "))
            .expect("mine");
        assert!(events.is_empty());
        let Action::Reply { text } = &actions[0] else {
            panic!("expected a reply, got {:?}", actions[0]);
        };
        assert!(text.contains("/fork"));
    }

    /// A seed in flight is a fork that exists and cannot run yet. `Ready` is
    /// what says it can, and it must clear the entry — a fork left pending
    /// would be started twice by anything that re-drives them.
    #[test]
    fn a_seed_that_lands_clears_the_pending_entry() {
        let mut c = cap();
        let fork = fork_via(&mut c, caller(), FORK_COMMAND);
        assert!(c.pending.contains_key(&fork));

        let (events, actions) = c
            .handle(caller(), &Message::Child(ChildMsg::Ready { child: fork }))
            .expect("mine");
        assert!(actions.is_empty());
        c.apply(&events[0]);
        assert!(c.pending.is_empty());
    }

    /// A seed that never landed is recorded, and nothing is delivered: the
    /// failure is that fork's own status, not a report owed to its source.
    #[test]
    fn a_seed_that_fails_is_recorded_and_delivers_nothing() {
        let mut c = cap();
        let fork = fork_via(&mut c, caller(), FORK_COMMAND);
        let (events, actions) = c
            .handle(
                caller(),
                &Message::Child(ChildMsg::Failed {
                    child: fork,
                    error: "the copy failed".into(),
                }),
            )
            .expect("mine");
        assert!(actions.is_empty());
        let CapEvent::Fork(Event::SeedFailed { error, .. }) = &events[0] else {
            panic!("expected a seed failure, got {:?}", events[0]);
        };
        assert_eq!(error, "the copy failed");
        c.apply(&events[0]);
        assert!(c.pending.is_empty());
    }

    /// A fork this capability did not create is not its business.
    #[test]
    fn a_child_i_did_not_create_is_not_mine() {
        let c = cap();
        assert!(
            c.handle(
                caller(),
                &Message::Child(ChildMsg::Ready {
                    child: RunnerId::new_v4()
                }),
            )
            .is_none()
        );
    }

    /// A fork owes nobody a result. There is no `ChildOutcome::Fork` to match,
    /// and an outcome addressed here — even for a fork this capability holds —
    /// belongs to whichever capability created a child that does report.
    #[test]
    fn an_outcome_is_never_a_forks_business() {
        let mut c = cap();
        let fork = fork_via(&mut c, caller(), FORK_COMMAND);
        assert!(
            c.handle(
                caller(),
                &Message::Child(ChildMsg::Outcome {
                    child: fork,
                    outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                        label: "l".into(),
                        report: "r".into(),
                    }),
                }),
            )
            .is_none()
        );
    }

    /// It equips nothing at all: `/fork` is typed, and a tool for it would let
    /// a model branch the conversation it is having.
    #[test]
    fn it_equips_no_tool() {
        let mut spec = AgentSpec::default();
        cap().setup(&mut spec);
        assert!(spec.layers.is_empty());
        assert!(spec.prompt.is_empty());
        assert!(!spec.has(&ToolLayer::Runtime));
    }

    /// Another built-in — `/compact` — belongs to a different capability, so
    /// the offer has to pass through this one.
    #[test]
    fn another_command_is_not_mine() {
        let c = cap();
        assert!(c.handle(caller(), &command("compact", "")).is_none());
        assert!(
            c.handle(caller(), &tool("bash", serde_json::json!({})))
                .is_none()
        );
    }
}
