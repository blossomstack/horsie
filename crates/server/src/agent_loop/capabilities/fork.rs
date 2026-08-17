//! `/fork` and `/summary-n-fork`: branching a conversation.
//!
//! The one capability with no tool. A fork is asked for by a person typing a
//! built-in, never by a model calling something, so it claims a
//! [`Msg::Command`] and advertises nothing — a tool for it would offer the
//! model a button it has no business pressing, and [`Capability::tools`] is
//! left at its empty default to say so.
//!
//! What it creates is a [`RunnerKind::Conversation`], not a kind of its own: a
//! fork *is* a conversation, one that carries a branch point. That collapses
//! the five fork-shaped events the previous shape needed — created, seeded,
//! titled, status-changed, turn-ended — into the conversation vocabulary every
//! runner already speaks.
//!
//! And it takes no [`ChildOutcome`] at all. A fork owes nobody a result, so it
//! reaches its creator through `Ready`/`Failed` only; there is no `Fork` arm to
//! match, which makes "a fork reports a result" unwritable rather than
//! something a reviewer has to notice. [`ForkCapability::pending`] is therefore
//! only ever a seed in flight — the copy or the summary that has to land before
//! the new conversation can run — and empties when it lands.
//!
//! For the same reason **a fork never holds this agent's conclusion**. Invariant
//! 6 is about children a report is owed by, and a fork owes none: the agent that
//! branched is free to finish while its branch runs on. That asymmetry is the
//! whole difference between this capability and [`super::sub_agent`], which
//! otherwise has the identical shape.
//!
//! # Answering a person who typed a slash command
//!
//! [`Act::Answer`] carries a `call`, because everything else that produces text
//! is a tool result. A built-in has no `tool_use` id, so the two answers this
//! capability makes use the only stable keys there are: the command's own name
//! for a refusal made before anything was minted, and the new conversation's
//! [`RunnerId`] for everything after — which is also the dedupe key the session
//! recognises a replayed [`SessionRequest::StartRunner`] by.

use super::{Act, CapEvent, CapSlice, Capability, Decision, Msg, SessionReply, SessionRequest};
use crate::sessions::forks::ForkMode;
use crate::sessions::runners::action::{Branch, RunnerArgs};
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::message::{ChildMsg, Command};
use crate::sessions::spec::AgentSettings;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The built-in that copies the source's log.
pub const FORK_COMMAND: &str = "fork";
/// The built-in that summarises it first.
pub const SUMMARY_COMMAND: &str = "summary-n-fork";

/// One agent's branches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkCapability {
    /// The agent this belongs to, and so the log a branch is cut from.
    ///
    /// Held rather than derived because [`Branch`] names it and a capability is
    /// handed no caller: the message that reaches `handle` says what was typed,
    /// never who by.
    pub agent: AgentId,
    /// What a fork inherits. Fixed when this agent was equipped, so two forks
    /// of one conversation cannot end up equipped differently.
    pub settings: AgentSettings,
    /// Forks asked of the session and not yet answered: the key the reply
    /// carries, and the conversation it named.
    pub requested: BTreeMap<String, RunnerId>,
    /// Forks whose seed has not landed yet. A fork with a seed in flight exists
    /// but cannot run.
    pub pending: BTreeSet<RunnerId>,
}

/// What this capability records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// A branch was asked of the session, and this is the conversation it
    /// named.
    Requested { call: String, fork: RunnerId },
    /// The session created it, and its seed is now in flight.
    Created { call: String },
    /// The session would not create it. Journaled because the
    /// [`Event::Requested`] before it was — this retracts a fact, and is not
    /// itself the refusal.
    Dropped { call: String },
    /// The seed landed, so the fork is a conversation like any other.
    Seeded { fork: RunnerId },
    /// The seed never landed. Recorded rather than delivered: the failure is
    /// that fork's own status, not a report owed to the log it was cut from.
    SeedFailed { fork: RunnerId, error: String },
}

impl ForkCapability {
    #[must_use]
    pub fn new(agent: AgentId, settings: AgentSettings) -> Self {
        Self {
            agent,
            settings,
            requested: BTreeMap::new(),
            pending: BTreeSet::new(),
        }
    }

    /// Somebody typed a built-in.
    fn commanded(&self, c: &Command) -> Option<Decision> {
        let mode = match c.name.as_str() {
            FORK_COMMAND => ForkMode::Copy,
            SUMMARY_COMMAND => ForkMode::Summary,
            _ => return None,
        };
        let message = c.args.trim();
        // A fork with nothing to do is the one refusal this capability makes on
        // its own: it would branch a whole conversation and then sit idle, and
        // the person would have to notice that themselves. Nothing is minted
        // yet, so the command's own name is what the answer is keyed by.
        if message.is_empty() {
            return Some(Decision::reply(
                &format!("/{}", c.name),
                format!(
                    "/{} needs a message saying what the new conversation should do",
                    c.name
                ),
            ));
        }
        let fork = RunnerId::new_v4();
        // Minted here, beside the runner id, because a fork has to be
        // addressable before its agent exists: the answer to `/fork` names this
        // agent, and so does the fork's row in the session list. Two ids and
        // not one — a runner and an agent are separate spaces, and a workflow
        // runner owns many agents, so an equality would hold here and be false
        // there.
        let agent = AgentId::new_v4();
        let call = fork.to_string();
        Some(
            Decision::record(vec![CapEvent::Fork(Event::Requested {
                call: call.clone(),
                fork,
            })])
            .then(Act::Ask(SessionRequest::StartRunner {
                call,
                id: fork,
                kind: RunnerKind::Conversation,
                args: Box::new(RunnerArgs::Conversation {
                    agent,
                    seed: Some(Branch {
                        source: self.agent,
                        // Zero, not a guess: the branch point is wherever this
                        // agent's log actually ends when the session cuts it,
                        // and the session stamps it there. A number chosen here
                        // would be one taken before the cut.
                        source_seq: 0,
                        mode,
                    }),
                    message: message.to_string(),
                    settings: Box::new(self.settings.clone()),
                }),
            })),
        )
    }

    /// The session answered a branch this capability asked for.
    fn replied(&self, reply: &SessionReply) -> Option<Decision> {
        let fork = *self.requested.get(reply.call())?;
        Some(match reply {
            SessionReply::Done { call } => {
                Decision::record(vec![CapEvent::Fork(Event::Created { call: call.clone() })]).then(
                    Act::Answer {
                        call: call.clone(),
                        text: format!("Forked: {fork}"),
                    },
                )
            }
            // A refusal the person cannot see is a command that never answered,
            // which is the same failure a tool call that never returns is.
            SessionReply::Refused { call, reason } => Decision::record(vec![CapEvent::Fork(
                Event::Dropped { call: call.clone() },
            )])
            .then(Act::Answer {
                call: call.clone(),
                text: reason.clone(),
            }),
        })
    }

    /// A fork moved.
    fn child(&self, m: &ChildMsg) -> Option<Decision> {
        match m {
            // The seed landed; the fork is a conversation like any other, and
            // its own runner starts its agent.
            ChildMsg::Ready { child } => self
                .pending
                .contains(child)
                .then(|| Decision::record(vec![CapEvent::Fork(Event::Seeded { fork: *child })])),
            // Nothing is delivered: a fork owes nobody a result, so a failed
            // one is recorded and shown as that fork's own status rather than
            // sent back to the conversation it branched from.
            ChildMsg::Failed { child, error } => self.pending.contains(child).then(|| {
                Decision::record(vec![CapEvent::Fork(Event::SeedFailed {
                    fork: *child,
                    error: error.clone(),
                })])
            }),
            // There is no `ChildOutcome::Fork`, so an outcome addressed here —
            // even for a fork this capability holds — belongs to whichever
            // capability created a child that does report.
            ChildMsg::Outcome { .. } => None,
        }
    }
}

#[async_trait::async_trait]
impl Capability for ForkCapability {
    fn name(&self) -> &'static str {
        "fork"
    }

    // `setup` and `tools` are both left at their defaults, which is the whole
    // of "a fork equips nothing". No tool, because `/fork` is typed rather than
    // called; and no prompt section either, because a paragraph about a command
    // the model cannot use spends context to tell it about something it will
    // never do.
    //
    // In particular not the "you are a fork" paragraph. This capability is held
    // by a conversation that *can* branch, which is every conversation; the
    // paragraph is for one that *is* a branch, and that is
    // [`super::title::TitleCapability`] — the only capability whose presence
    // means "this agent is a fork", and the one that owns the tool the
    // paragraph tells it to call.

    fn handle(&self, msg: &Msg) -> Option<Decision> {
        match msg {
            Msg::Command(c) => self.commanded(c),
            Msg::Reply(reply) => self.replied(reply),
            Msg::Child(m) => self.child(m),
            // A fork owes nobody a result, so it never holds a conclusion — see
            // the module doc. The agent that branched is free to finish while
            // its branch runs on.
            Msg::Tool(_) | Msg::Turn(_) | Msg::Answer(_) => None,
        }
    }

    fn apply(&mut self, event: &CapEvent) {
        // `let ... else` rather than a match with an arm per sibling: every
        // capability is offered every event, and listing the others here would
        // make adding one a change to all of them.
        let CapEvent::Fork(event) = event else {
            return;
        };
        match event {
            Event::Requested { call, fork } => {
                self.requested.insert(call.clone(), *fork);
            }
            Event::Created { call } => {
                if let Some(fork) = self.requested.remove(call) {
                    self.pending.insert(fork);
                }
            }
            Event::Dropped { call } => {
                self.requested.remove(call);
            }
            // Both endings clear it: `pending` records a seed in flight, and a
            // seed that failed is no longer in flight either.
            Event::Seeded { fork } | Event::SeedFailed { fork, .. } => {
                self.pending.remove(fork);
            }
        }
    }

    fn save(&self) -> CapSlice {
        CapSlice::Fork(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::{Capabilities, TurnEvent};
    use crate::sessions::runners::capabilities::testing::settings;
    use crate::sessions::runners::message::{ChildOutcome, SubAgentOutcome, ToolCall};

    fn cap() -> ForkCapability {
        ForkCapability::new(AgentId::new_v4(), settings())
    }

    fn command(name: &str, args: &str) -> Command {
        Command {
            name: name.into(),
            args: args.into(),
        }
    }

    fn fold(c: &mut ForkCapability, d: &Decision) {
        for event in &d.events {
            c.apply(event);
        }
    }

    /// Branch, and let the session say yes — the only way there is to a seed in
    /// flight.
    fn forked(c: &mut ForkCapability, name: &str) -> RunnerId {
        let d = c
            .handle(&Msg::Command(&command(name, "look into it")))
            .expect("mine");
        fold(c, &d);
        let [Act::Ask(SessionRequest::StartRunner { id, .. })] = d.acts.as_slice() else {
            panic!("expected an ask, got {:?}", d.acts);
        };
        let fork = *id;
        let d = c
            .handle(&Msg::Reply(&SessionReply::Done {
                call: fork.to_string(),
            }))
            .expect("mine");
        fold(c, &d);
        fork
    }

    /// A fork is a conversation with a branch point, and the branch names the
    /// agent whose log it was cut from. Getting that wrong is how a fork of a
    /// fork used to read as a fork of something else entirely.
    #[test]
    fn forking_asks_for_a_conversation_branched_from_this_agent() {
        let mut c = cap();
        let d = c
            .handle(&Msg::Command(&command(
                FORK_COMMAND,
                "  look into the flake  ",
            )))
            .expect("mine");

        let [CapEvent::Fork(Event::Requested { call, fork })] = d.events.as_slice() else {
            panic!("expected one Requested event, got {:?}", d.events);
        };
        let [
            Act::Ask(SessionRequest::StartRunner {
                call: asked,
                id,
                kind,
                args,
            }),
        ] = d.acts.as_slice()
        else {
            panic!("expected an ask, got {:?}", d.acts);
        };
        assert_eq!(id, fork, "the log records a fork nothing was asked for");
        assert_eq!(asked, call);
        assert_eq!(
            *asked,
            fork.to_string(),
            "the dedupe key has to name the fork a replay would re-ask for"
        );
        assert_eq!(*kind, RunnerKind::Conversation);
        let RunnerArgs::Conversation {
            agent,
            seed,
            message,
            ..
        } = args.as_ref()
        else {
            panic!("expected conversation args, got {args:?}");
        };
        // The fork's agent is decided here, with its runner, because the answer
        // to `/fork` names it — and it is its *own* id, not the runner's. Two
        // spaces on purpose: a workflow runner owns many agents, so an equality
        // that held for a fork would be false for a run.
        assert_ne!(agent.as_uuid(), fork.as_uuid());
        assert_eq!(message, "look into the flake");
        let seed = seed.as_ref().expect("a fork has a branch point");
        assert_eq!(seed.source, c.agent, "the branch names the wrong log");
        assert_eq!(seed.mode, ForkMode::Copy);

        // Nothing is pending yet: the session has not said the fork exists.
        fold(&mut c, &d);
        assert!(c.pending.is_empty());
        assert_eq!(c.requested.get(&fork.to_string()), Some(fork));
    }

    /// The two built-ins differ only in how the new conversation is seeded, so
    /// one handler serves both and the mode is the whole difference.
    #[test]
    fn summary_n_fork_seeds_with_a_summary() {
        let d = cap()
            .handle(&Msg::Command(&command(
                SUMMARY_COMMAND,
                "carry on elsewhere",
            )))
            .expect("mine");
        let [Act::Ask(SessionRequest::StartRunner { args, .. })] = d.acts.as_slice() else {
            panic!("expected an ask, got {:?}", d.acts);
        };
        let RunnerArgs::Conversation { seed, .. } = args.as_ref() else {
            panic!("expected conversation args, got {args:?}");
        };
        assert_eq!(seed.as_ref().map(|s| s.mode), Some(ForkMode::Summary));
    }

    /// An empty message is refused in words and journals nothing: a fork with
    /// nothing to do would branch a conversation and then sit there.
    #[test]
    fn an_empty_message_is_refused_and_journals_nothing() {
        let d = cap()
            .handle(&Msg::Command(&command(FORK_COMMAND, "   ")))
            .expect("mine");
        assert!(
            d.events.is_empty(),
            "a refusal is not a fact about the agent"
        );
        let [Act::Answer { call, text }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert!(text.contains("/fork"));
        assert_eq!(
            call, "/fork",
            "a built-in has no tool_use id, so its own name is the key"
        );
        assert!(
            !d.acts.iter().any(|a| matches!(a, Act::Ask(_))),
            "a refused fork must not reach the session"
        );
    }

    /// The session refusing is the other refusal, and it has to reach the
    /// person who typed the command — otherwise `/fork` answered nothing.
    #[test]
    fn a_refusal_from_the_session_reaches_the_person_and_retracts_the_intent() {
        let mut c = cap();
        let d = c
            .handle(&Msg::Command(&command(FORK_COMMAND, "look into it")))
            .expect("mine");
        fold(&mut c, &d);
        let [Act::Ask(SessionRequest::StartRunner { call, .. })] = d.acts.as_slice() else {
            panic!("expected an ask, got {:?}", d.acts);
        };
        let call = call.clone();

        let d = c
            .handle(&Msg::Reply(&SessionReply::Refused {
                call: call.clone(),
                reason: "this session cannot be forked".into(),
            }))
            .expect("the reply answers a request I made");
        let [Act::Answer { text, .. }] = d.acts.as_slice() else {
            panic!("expected one answer, got {:?}", d.acts);
        };
        assert_eq!(text, "this session cannot be forked");

        fold(&mut c, &d);
        assert!(c.requested.is_empty(), "the intent was not retracted");
        assert!(c.pending.is_empty(), "a refused fork is not in flight");
    }

    /// A reply for a request this capability never made belongs to whichever
    /// capability did make it.
    #[test]
    fn a_reply_for_a_request_i_never_made_is_not_mine() {
        assert!(
            cap()
                .handle(&Msg::Reply(&SessionReply::Done {
                    call: "someone-else".into()
                }))
                .is_none()
        );
    }

    /// A seed in flight is a fork that exists and cannot run yet. `Ready` is
    /// what says it can, and it must clear the entry — a fork left pending
    /// would be started twice by anything that re-drives them.
    #[test]
    fn a_seed_that_lands_clears_the_pending_entry() {
        let mut c = cap();
        let fork = forked(&mut c, FORK_COMMAND);
        assert!(c.pending.contains(&fork));

        let d = c
            .handle(&Msg::Child(&ChildMsg::Ready { child: fork }))
            .expect("mine");
        assert!(d.acts.is_empty());
        fold(&mut c, &d);
        assert!(c.pending.is_empty());
    }

    /// A seed that never landed is recorded, and nothing is delivered: the
    /// failure is that fork's own status, not a report owed to its source.
    #[test]
    fn a_seed_that_fails_is_recorded_and_delivers_nothing() {
        let mut c = cap();
        let fork = forked(&mut c, FORK_COMMAND);
        let d = c
            .handle(&Msg::Child(&ChildMsg::Failed {
                child: fork,
                error: "the copy failed".into(),
            }))
            .expect("mine");
        assert!(d.acts.is_empty());
        let [CapEvent::Fork(Event::SeedFailed { error, .. })] = d.events.as_slice() else {
            panic!("expected a seed failure, got {:?}", d.events);
        };
        assert_eq!(error, "the copy failed");
        fold(&mut c, &d);
        assert!(c.pending.is_empty());
    }

    /// A fork this capability did not create is not its business.
    #[test]
    fn a_child_i_did_not_create_is_not_mine() {
        assert!(
            cap()
                .handle(&Msg::Child(&ChildMsg::Ready {
                    child: RunnerId::new_v4()
                }))
                .is_none()
        );
    }

    /// A fork owes nobody a result. There is no `ChildOutcome::Fork` to match,
    /// and an outcome addressed here — even for a fork this capability holds —
    /// belongs to whichever capability created a child that does report.
    #[test]
    fn an_outcome_is_never_a_forks_business() {
        let mut c = cap();
        let fork = forked(&mut c, FORK_COMMAND);
        assert!(
            c.handle(&Msg::Child(&ChildMsg::Outcome {
                child: fork,
                outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                    label: "l".into(),
                    report: "r".into(),
                }),
            }))
            .is_none()
        );
    }

    /// **A fork never holds this agent's conclusion.** Invariant 6 is about
    /// children a report is owed by, and a fork owes none — so an agent that
    /// branched is free to finish while its branch runs on. This is the one
    /// place this capability and `sub_agent` deliberately differ.
    #[test]
    fn a_fork_in_flight_does_not_hold_the_conclusion() {
        let mut c = cap();
        let _ = forked(&mut c, FORK_COMMAND);
        assert!(!c.pending.is_empty());
        for boundary in [
            TurnEvent::Began,
            TurnEvent::Ended,
            TurnEvent::Failed,
            TurnEvent::Cancelled,
        ] {
            assert!(
                c.handle(&Msg::Turn(boundary)).is_none(),
                "{boundary:?} was claimed by a capability nobody is waiting on"
            );
        }
    }

    /// It advertises nothing at all: `/fork` is typed, and a tool for it would
    /// let a model branch the conversation it is having.
    #[test]
    fn it_advertises_no_tool() {
        assert!(cap().tools().is_empty());
    }

    /// A seed in flight is what says a fork exists and cannot run yet, so
    /// losing it in the journal leaves a conversation stuck in `Provisioning`
    /// with nobody left to finish it.
    #[test]
    fn a_seed_in_flight_survives_a_slice_round_trip() {
        let mut c = cap();
        let fork = forked(&mut c, FORK_COMMAND);
        let source = c.agent;
        let caps = Capabilities::new(vec![Box::new(c)]);

        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let CapSlice::Fork(back) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(
            back.pending.into_iter().collect::<Vec<_>>(),
            vec![fork],
            "the reload was rebuilt from config and lost the seed in flight"
        );
        assert_eq!(
            back.agent, source,
            "a reload that forgot whose log this is would branch from nowhere"
        );
    }

    /// Another built-in — `/compact` — belongs to a different capability, so
    /// the offer has to pass through this one.
    #[test]
    fn another_message_is_not_mine() {
        let c = cap();
        assert!(c.handle(&Msg::Command(&command("compact", ""))).is_none());
        assert!(
            c.handle(&Msg::Tool(&ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }))
            .is_none()
        );
        assert!(c.handle(&Msg::Answer(&[])).is_none());
    }
}
