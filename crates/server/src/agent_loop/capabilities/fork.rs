//! `/fork` and `/summary-n-fork`: branching a conversation.
//!
//! The one capability with no tool. A fork is asked for by a person typing a
//! built-in, never by a model calling something, so nothing here is
//! advertised — a tool for it would offer the model a button it has no
//! business pressing, so [`super::Capability::claims`] is empty for this arm
//! and it contributes nothing to the composed toolbox at all.
//!
//! It is also the one command with no run waiting on it, which is why the
//! agent's own `Fork` command carries no [`Answering`](super::Answering). A
//! person typing a built-in is not a dangling `tool_use`, and the two sentences
//! below are keyed by the only stable names there are rather than by a
//! `tool_use` id.
//!
//! What it creates is a [`RunnerKind::Conversation`], not a kind of its own: a
//! fork *is* a conversation, one that carries a branch point. That collapses
//! the five fork-shaped events the previous shape needed — created, seeded,
//! titled, status-changed, turn-ended — into the conversation vocabulary every
//! runner already speaks.
//!
//! And it takes no child outcome at all. A fork owes nobody a result, so it
//! reaches its creator through `Ready`/`Failed` only; there is no `Fork` arm of
//! [`ChildOutcome`](crate::sessions::runners::message::ChildOutcome) to match,
//! which makes "a fork reports a result" unwritable rather than something a
//! reviewer has to notice. A [`Seed`] is therefore only ever a seed in
//! flight — the copy or the summary that has to land before the new
//! conversation can run — and [`ForkState`] empties when it lands.
//!
//! For the same reason **a fork never holds this agent's conclusion**. Invariant
//! 6 is about children a report is owed by, and a fork owes none: the agent that
//! branched is free to finish while its branch runs on. That asymmetry is the
//! whole difference between this capability and [`super::sub_agent`], which
//! otherwise has the identical shape.
//!
//! # What this file decides, and what the actor does with it
//!
//! Nothing here returns an event. Each function answers one narrow
//! question — [`Branched`] for a built-in somebody typed, [`Branch`] for what
//! the session said about one, [`Seed`] for a fork's seed settling — and the
//! actor's arm is what turns the answer into an
//! [`AgentDomainEvent`](crate::agent_loop::state::AgentDomainEvent), journals
//! it, and only then sends the request. So "the request was recorded before it
//! went out" is a property of one place rather than of every capability, and a
//! capability cannot make a fact durable by deciding it.
//!
//! # Answering a person who typed a slash command
//!
//! A person typing `/fork` is waiting on the answer, so both sentences this
//! capability makes go back to them: the refusal in [`Branched::Told`] answers
//! immediately, and everything after is keyed by the new conversation's
//! [`RunnerId`] — which is also the dedupe key the session recognises a
//! replayed [`StartRunner`](super::SessionRequest::StartRunner) by.
//!
//! Neither sentence reaches anybody today. A typed built-in has no run waiting
//! on it, so the actor records that it had nothing to answer and drops the
//! text. The words are said here all the same: what a person is told is this
//! capability's to decide, and only the delivery is missing.

use super::{SessionReply, SessionRequest};
use crate::sessions::forks::ForkMode;
use crate::sessions::runners::action::RunnerArgs;
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::message::ChildMsg;
use crate::sessions::spec::AgentSettings;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The built-in that copies the source's log.
pub const FORK_COMMAND: &str = "fork";
/// The built-in that summarises it first.
pub const SUMMARY_COMMAND: &str = "summary-n-fork";

/// Which built-in asks for this mode.
///
/// The mode is decided by whoever parsed the line, so the two names are one
/// path through this file rather than two: what differs between `/fork` and
/// `/summary-n-fork` is only how the new conversation is seeded.
///
/// The name is the only stable key a refusal made before anything is minted can
/// be answered by.
#[must_use]
pub(crate) fn built_in(mode: ForkMode) -> &'static str {
    match mode {
        ForkMode::Copy => FORK_COMMAND,
        ForkMode::Summary => SUMMARY_COMMAND,
    }
}

/// One agent's branches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkCapability {
    /// The agent this belongs to, and so the log a branch is cut from.
    ///
    /// Held rather than derived because the branch point names it and a
    /// capability is handed no caller: what reaches [`Self::branched`] says what
    /// was typed, never who by.
    pub agent: AgentId,
    /// What a fork inherits. Fixed when this agent was equipped, so two forks
    /// of one conversation cannot end up equipped differently.
    pub settings: AgentSettings,
}

/// The branches this agent has in flight.
///
/// Fields private to this file: a request the session has not answered and a
/// seed that has not landed are both this capability's own bookkeeping, and
/// nothing outside decides what either means.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForkState {
    /// Forks asked of the session and not yet answered, by the key the reply
    /// carries.
    ///
    /// Journaled *before* the ask goes out, so a crash in the window replays as
    /// an intent [`ForkCapability::reloaded`] asks about again, naming the same
    /// conversation.
    #[serde(default)]
    requested: BTreeMap<String, Pending>,
    /// Forks whose seed has not landed yet. A fork with a seed in flight exists
    /// but cannot run.
    ///
    /// Named for what it holds rather than "pending", because this capability
    /// has two in-flight things and only one of them is a seed.
    #[serde(default)]
    seeding: BTreeSet<RunnerId>,
}

impl ForkState {
    /// A branch was asked of the session.
    pub(crate) fn requested(&mut self, call: String, pending: Pending) {
        self.requested.insert(call, pending);
    }

    /// The session created it, and its seed is now in flight.
    pub(crate) fn created(&mut self, call: &str) {
        if let Some(pending) = self.requested.remove(call) {
            self.seeding.insert(pending.fork);
        }
    }

    /// The session would not create it.
    pub(crate) fn dropped(&mut self, call: &str) {
        self.requested.remove(call);
    }

    /// The seed landed, or it never will. Both stop it being in flight.
    pub(crate) fn settled(&mut self, fork: RunnerId) {
        self.seeding.remove(&fork);
    }
}

#[cfg(test)]
/// What this state holds, for the tests that assert on it.
///
/// `#[cfg(test)]` because nothing in production reads it: the decisions that
/// need it are in this file and take it by reference. An accessor kept for a
/// caller that does not exist is how a private field stops being private.
impl ForkState {
    /// The branches the session has not answered yet.
    #[must_use]
    pub(crate) fn pending(&self) -> &BTreeMap<String, Pending> {
        &self.requested
    }

    /// The forks whose seed is still in flight.
    #[must_use]
    pub(crate) fn seeding(&self) -> &BTreeSet<RunnerId> {
        &self.seeding
    }
}

/// One branch asked of the session and not yet answered.
///
/// The whole request rather than its ids alone, because a re-ask on load has to
/// send the *same* request again — and the message a person typed is the only
/// thing that says what the new conversation is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The conversation the branch will be.
    pub fork: RunnerId,
    /// The fork's own agent, and the session's dedupe key: a replayed request
    /// carries the same id, so the session recognises a conversation it has
    /// already created instead of branching twice.
    pub agent: AgentId,
    pub message: String,
    pub mode: ForkMode,
}

/// What a typed `/fork` came to.
#[derive(Debug)]
pub(crate) enum Branched {
    /// A fork with nothing to do. Nothing is minted, so there is nothing to
    /// key it by — the words go straight back to whoever typed the command.
    Told { reason: String },
    /// Journal the request and put it to the session.
    Ask { call: String, pending: Pending },
}

/// What this capability decides.
impl ForkCapability {
    #[must_use]
    pub fn new(agent: AgentId, settings: AgentSettings) -> Self {
        Self { agent, settings }
    }

    /// Somebody typed a built-in.
    #[must_use]
    pub(crate) fn branched(&self, mode: ForkMode, message: &str) -> Branched {
        let message = message.trim();
        // A fork with nothing to do is the one refusal this capability makes on
        // its own: it would branch a whole conversation and then sit idle, and
        // the person would have to notice that themselves. Nothing is minted
        // yet, so the built-in's own name is what the answer is keyed by.
        if message.is_empty() {
            return Branched::Told {
                reason: format!(
                    "/{} needs a message saying what the new conversation should do",
                    built_in(mode)
                ),
            };
        }
        // The fork's agent is minted here, beside the runner id, because a fork
        // has to be addressable before its agent exists: the answer to `/fork`
        // names this agent, and so does the fork's row in the session list. Two
        // ids and not one — a runner and an agent are separate spaces, and a
        // workflow runner owns many agents, so an equality would hold here and
        // be false there.
        let pending = Pending {
            fork: RunnerId::new_v4(),
            agent: AgentId::new_v4(),
            message: message.to_string(),
            mode,
        };
        Branched::Ask {
            call: pending.fork.to_string(),
            pending,
        }
    }

    /// The request a [`Pending`] names.
    ///
    /// One function and two callers — the branch, and the re-ask on load —
    /// because the second has to send exactly what the first sent. Built from
    /// the journaled request plus this capability's own config, so nothing in
    /// it is minted twice.
    #[must_use]
    pub(crate) fn request(&self, call: &str, pending: &Pending) -> SessionRequest {
        SessionRequest::StartRunner {
            call: call.to_string(),
            id: pending.fork,
            kind: RunnerKind::Conversation,
            args: Box::new(RunnerArgs::Conversation {
                agent: pending.agent,
                seed: Some(crate::sessions::runners::action::Branch {
                    source: self.agent,
                    // Zero, not a guess: the branch point is wherever this
                    // agent's log actually ends when the session cuts it, and
                    // the session stamps it there. A number chosen here would be
                    // one taken before the cut.
                    source_seq: 0,
                    mode: pending.mode,
                }),
                message: pending.message.clone(),
                settings: Box::new(self.settings.clone()),
            }),
        }
    }

    /// Everything asked for and never answered, asked again. Empty when there
    /// is nothing outstanding.
    ///
    /// A request still in the fold is one the dead process may never have sent,
    /// and the person who typed `/fork` was never told anything. Re-asked with
    /// the ids already recorded, so the session can tell a repeat from a second
    /// branch.
    ///
    /// Requests and nothing else, so there is nothing to journal here: the
    /// `ForkRequested` this reads is still the only fact, and a second copy
    /// would say a second fork was wanted.
    #[must_use]
    pub(crate) fn reloaded(&self, state: &ForkState) -> Vec<SessionRequest> {
        state
            .requested
            .iter()
            .map(|(call, pending)| self.request(call, pending))
            .collect()
    }
}

/// What the session said about a branch this agent asked for.
#[derive(Debug)]
pub(crate) enum Branch {
    Created { call: String, fork: RunnerId },
    Dropped { call: String, reason: String },
}

impl Branch {
    #[must_use]
    pub(crate) fn call(&self) -> &str {
        match self {
            Self::Created { call, .. } | Self::Dropped { call, .. } => call,
        }
    }

    /// What the person who typed the built-in would be told.
    ///
    /// "Would": a built-in has no run waiting on it, so the actor has nowhere
    /// to send this and says so instead. The sentence is this capability's all
    /// the same — what a branch means to the person who asked for it is not
    /// something the actor should be inventing when the delivery arrives.
    #[must_use]
    pub(crate) fn told(&self) -> String {
        match self {
            Self::Created { fork, .. } => format!("Forked: {fork}"),
            // A refusal the person cannot see is a command that never answered,
            // which is the same failure a tool call that never returns is.
            Self::Dropped { reason, .. } => reason.clone(),
        }
    }
}

/// The session answered a branch this capability asked for.
///
/// `None` when this reply answers something that is not a branch of ours.
#[must_use]
pub(crate) fn replied(state: &ForkState, reply: &SessionReply) -> Option<Branch> {
    let fork = state.requested.get(reply.call())?.fork;
    Some(match reply {
        SessionReply::Done { call } => Branch::Created {
            call: call.clone(),
            fork,
        },
        SessionReply::Refused { call, reason } => Branch::Dropped {
            call: call.clone(),
            reason: reason.clone(),
        },
    })
}

/// A fork's seed settled.
#[derive(Debug)]
pub enum Seed {
    Landed { fork: RunnerId },
    Failed { fork: RunnerId, error: String },
}

/// A fork moved.
///
/// `None` for anything this capability is not holding a seed for.
///
/// Sent by the session when a branch point settles, one way or the other. This
/// is the only thing that ever clears [`ForkState::seeding`], so an agent that
/// was never told would carry every branch it has ever taken as still in
/// flight, for the life of its journal.
#[must_use]
pub fn child(state: &ForkState, m: &ChildMsg) -> Option<Seed> {
    match m {
        // The seed landed; the fork is a conversation like any other, and its
        // own runner starts its agent.
        ChildMsg::Ready { child } => state
            .seeding
            .contains(child)
            .then_some(Seed::Landed { fork: *child }),
        // Nothing is delivered: a fork owes nobody a result, so a failed one is
        // recorded and shown as that fork's own status rather than sent back to
        // the conversation it branched from.
        ChildMsg::Failed { child, error } => state.seeding.contains(child).then(|| Seed::Failed {
            fork: *child,
            error: error.clone(),
        }),
        // There is no `ChildOutcome::Fork`, so an outcome addressed here — even
        // for a fork this capability holds — belongs to whichever capability
        // created a child that does report.
        ChildMsg::Outcome { .. } => None,
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl ForkCapability {
    pub fn name(&self) -> &'static str {
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::testing::{advertised_by, facts, settings};
    use crate::agent_loop::capabilities::{Capabilities, Capability};
    use crate::agent_loop::state::AgentDomainEvent;
    use crate::sessions::runners::message::{ChildOutcome, SubAgentOutcome};

    /// A conversation that can branch.
    fn cap() -> ForkCapability {
        ForkCapability::new(AgentId::new_v4(), settings())
    }

    /// Journal one event, the way the actor's arm does once it is durable.
    fn folded(fork: ForkState, event: AgentDomainEvent) -> ForkState {
        crate::agent_loop::AgentState {
            fork,
            ..Default::default()
        }
        .apply(event)
        .fork
    }

    /// The pair the actor holds: what this agent may do, and what it has
    /// folded. A capability decides from state it does not own, so the journal
    /// tests have to round-trip both halves together.
    fn holding(c: ForkCapability, fork: ForkState) -> crate::agent_loop::AgentState {
        crate::agent_loop::AgentState {
            capabilities: Capabilities::new(vec![Capability::Fork(c)]),
            fork,
            ..Default::default()
        }
    }

    /// The same, read back the way a new process reads it off the journal.
    fn reload(state: &crate::agent_loop::AgentState) -> (ForkCapability, ForkState) {
        let written = serde_json::to_string(state).expect("write");
        let back: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        let c = {
            let [Capability::Fork(c)] = back.capabilities.iter().collect::<Vec<_>>()[..] else {
                panic!("the journal changed which capability this is");
            };
            c.clone()
        };
        (c, back.fork)
    }

    /// Branch, and let the session say yes — the only way there is to a seed in
    /// flight.
    fn forked(c: &ForkCapability, mode: ForkMode) -> (ForkState, RunnerId) {
        let Branched::Ask { call, pending } = c.branched(mode, "look into it") else {
            panic!("expected an ask");
        };
        let fork = pending.fork;
        let state = folded(
            ForkState::default(),
            AgentDomainEvent::ForkRequested {
                call: call.clone(),
                pending,
            },
        );
        let branch = replied(&state, &SessionReply::Done { call }).expect("mine");
        assert_eq!(
            branch.told(),
            format!("Forked: {fork}"),
            "the person who typed the built-in is told which conversation it became"
        );
        (
            folded(
                state,
                AgentDomainEvent::ForkCreated {
                    call: branch.call().to_string(),
                },
            ),
            fork,
        )
    }

    /// A fork is a conversation with a branch point, and the branch names the
    /// agent whose log it was cut from. Getting that wrong is how a fork of a
    /// fork used to read as a fork of something else entirely.
    #[test]
    fn forking_asks_for_a_conversation_branched_from_this_agent() {
        let c = cap();
        let Branched::Ask { call, pending } = c.branched(ForkMode::Copy, "  look into the flake  ")
        else {
            panic!("expected a branch to be asked for");
        };
        assert_eq!(
            call,
            pending.fork.to_string(),
            "the dedupe key has to name the fork a replay would re-ask for"
        );

        let SessionRequest::StartRunner {
            call: asked,
            id,
            kind,
            args,
        } = c.request(&call, &pending)
        else {
            panic!("expected a runner to be asked for");
        };
        assert_eq!(
            id, pending.fork,
            "the log records a fork nothing was asked for"
        );
        assert_eq!(asked, call);
        assert_eq!(kind, RunnerKind::Conversation);
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
        assert_ne!(agent.as_uuid(), pending.fork.as_uuid());
        assert_eq!(
            *agent, pending.agent,
            "the agent asked for is not the one journaled, so a replay would \
             mint a second one"
        );
        assert_eq!(message, "look into the flake");
        let seed = seed.as_ref().expect("a fork has a branch point");
        assert_eq!(seed.source, c.agent, "the branch names the wrong log");
        assert_eq!(seed.mode, ForkMode::Copy);

        // Nothing is seeding yet: the session has not said the fork exists.
        let state = folded(
            ForkState::default(),
            AgentDomainEvent::ForkRequested {
                call,
                pending: pending.clone(),
            },
        );
        assert!(state.seeding().is_empty());
        assert_eq!(
            state.pending().get(&pending.fork.to_string()),
            Some(&pending)
        );
    }

    /// The two built-ins differ only in how the new conversation is seeded, so
    /// one path serves both and the mode is the whole difference.
    #[test]
    fn summary_n_fork_seeds_with_a_summary() {
        let c = cap();
        let Branched::Ask { call, pending } = c.branched(ForkMode::Summary, "carry on elsewhere")
        else {
            panic!("expected a branch to be asked for");
        };
        let SessionRequest::StartRunner { args, .. } = c.request(&call, &pending) else {
            panic!("expected a runner to be asked for");
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
        // `Told` carries no `Pending`, so there is nothing for the actor to
        // journal and nothing to put to the session: a refusal is not a fact
        // about the agent, and a refused fork must not reach the session.
        let Branched::Told { reason } = cap().branched(ForkMode::Copy, "   ") else {
            panic!("expected a refusal, not a branch");
        };
        assert!(
            reason.contains("/fork"),
            "the refusal names the command that was typed: {reason}"
        );
    }

    /// The session refusing is the other refusal, and it has to reach the
    /// person who typed the command — otherwise `/fork` answered nothing.
    #[test]
    fn a_refusal_from_the_session_reaches_the_person_and_retracts_the_intent() {
        let c = cap();
        let Branched::Ask { call, pending } = c.branched(ForkMode::Copy, "look into it") else {
            panic!("expected a branch to be asked for");
        };
        let state = folded(
            ForkState::default(),
            AgentDomainEvent::ForkRequested {
                call: call.clone(),
                pending,
            },
        );

        let branch = replied(
            &state,
            &SessionReply::Refused {
                call,
                reason: "this session cannot be forked".into(),
            },
        )
        .expect("the reply answers a request I made");
        assert_eq!(branch.told(), "this session cannot be forked");

        let state = folded(
            state,
            AgentDomainEvent::ForkDropped {
                call: branch.call().to_string(),
            },
        );
        assert!(state.pending().is_empty(), "the intent was not retracted");
        assert!(
            state.seeding().is_empty(),
            "a refused fork is not in flight"
        );
    }

    /// **The crash window.** A journal that stops between `ForkRequested` and
    /// the session's answer is a branch the session may never have heard of,
    /// and the person who typed `/fork` was told nothing at all. The load asks
    /// again, with the ids and the message the log already holds.
    #[test]
    fn a_branch_the_session_never_answered_is_asked_again_on_load() {
        let c = cap();
        let source = c.agent;
        let Branched::Ask { call, pending } = c.branched(ForkMode::Copy, "look into it") else {
            panic!("expected a branch to be asked for");
        };
        let (first_call, fork, branch_agent) = (call.clone(), pending.fork, pending.agent);
        let state = folded(
            ForkState::default(),
            AgentDomainEvent::ForkRequested { call, pending },
        );

        // The cut: nothing past the request is folded, and what comes back is
        // read off the journal the way a new process reads it.
        let (c, state) = reload(&holding(c, state));

        // Requests and nothing else: a re-ask is not a second branch, and there
        // is no event here for one to be recorded as.
        let asks = c.reloaded(&state);
        let [SessionRequest::StartRunner { call, id, args, .. }] = asks.as_slice() else {
            panic!("expected exactly one re-ask, got {asks:?}");
        };
        assert_eq!(*call, first_call, "the answer would reach nobody");
        assert_eq!(*id, fork, "the re-ask names a fork the log never recorded");
        let RunnerArgs::Conversation {
            agent,
            seed,
            message,
            ..
        } = args.as_ref()
        else {
            panic!("expected conversation args, got {args:?}");
        };
        assert_eq!(
            *agent, branch_agent,
            "a re-ask that mints a fresh agent is a second conversation the \
             session has no way to recognise"
        );
        assert_eq!(message, "look into it");
        let seed = seed.as_ref().expect("a fork has a branch point");
        assert_eq!(seed.source, source, "the re-ask branches from nowhere");
        assert_eq!(seed.mode, ForkMode::Copy);
    }

    /// And a branch the session already answered is not asked for again — a
    /// fork whose seed is in flight is not a fork nobody has heard of.
    #[test]
    fn a_branch_the_session_answered_is_not_asked_again() {
        let c = cap();
        let (state, _) = forked(&c, ForkMode::Copy);
        assert!(
            c.reloaded(&state).is_empty(),
            "the session already created this fork; asking again duplicates it"
        );
    }

    /// A reply for a request this capability never made belongs to whichever
    /// capability did make it.
    #[test]
    fn a_reply_for_a_request_i_never_made_is_not_mine() {
        let (state, _) = forked(&cap(), ForkMode::Copy);
        assert!(
            replied(
                &state,
                &SessionReply::Done {
                    call: "someone-else".into()
                }
            )
            .is_none()
        );
    }

    /// A seed in flight is a fork that exists and cannot run yet. `Ready` is
    /// what says it can, and it must clear the entry — a fork left pending
    /// would be started twice by anything that re-drives them.
    #[test]
    fn a_seed_that_lands_clears_the_pending_entry() {
        let c = cap();
        let (state, fork) = forked(&c, ForkMode::Copy);
        assert!(state.seeding().contains(&fork));

        let Some(Seed::Landed { fork: landed }) = child(&state, &ChildMsg::Ready { child: fork })
        else {
            panic!("expected the seed to have landed");
        };
        assert_eq!(landed, fork);
        let state = folded(state, AgentDomainEvent::ForkSeeded { fork: landed });
        assert!(state.seeding().is_empty());
    }

    /// A seed that never landed is recorded, and nothing is delivered: the
    /// failure is that fork's own status, not a report owed to its source.
    #[test]
    fn a_seed_that_fails_is_recorded_and_delivers_nothing() {
        let c = cap();
        let (state, fork) = forked(&c, ForkMode::Copy);
        let seed = child(
            &state,
            &ChildMsg::Failed {
                child: fork,
                error: "the copy failed".into(),
            },
        );
        // Nothing to deliver: a seed says it landed or it failed, and there is
        // no arm here for a result owed to anybody.
        let Some(Seed::Failed {
            fork: failed,
            error,
        }) = &seed
        else {
            panic!("expected a seed failure, got {seed:?}");
        };
        assert_eq!(error, "the copy failed");
        let state = folded(
            state,
            AgentDomainEvent::ForkSeedFailed {
                fork: *failed,
                error: error.clone(),
            },
        );
        assert!(state.seeding().is_empty());
    }

    /// A fork this capability did not create is not its business.
    #[test]
    fn a_child_i_did_not_create_is_not_mine() {
        let (state, _) = forked(&cap(), ForkMode::Copy);
        assert!(
            child(
                &state,
                &ChildMsg::Ready {
                    child: RunnerId::new_v4()
                }
            )
            .is_none()
        );
    }

    /// A fork owes nobody a result. There is no `ChildOutcome::Fork` to match,
    /// and an outcome addressed here — even for a fork this capability holds —
    /// belongs to whichever capability created a child that does report.
    #[test]
    fn an_outcome_is_never_a_forks_business() {
        let (state, fork) = forked(&cap(), ForkMode::Copy);
        assert!(
            child(
                &state,
                &ChildMsg::Outcome {
                    child: fork,
                    outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                        label: "l".into(),
                        report: "r".into(),
                    }),
                }
            )
            .is_none()
        );
    }

    /// It advertises nothing at all: `/fork` is typed, and a tool for it would
    /// let a model branch the conversation it is having.
    #[test]
    fn it_advertises_no_tool() {
        assert!(advertised_by(&Capability::Fork(cap()), &facts()).is_empty());
    }

    /// A seed in flight is what says a fork exists and cannot run yet, so
    /// losing it in the journal leaves a conversation stuck in `Provisioning`
    /// with nobody left to finish it.
    #[test]
    fn a_seed_in_flight_survives_the_journal_round_trip() {
        let c = cap();
        let source = c.agent;
        let (state, fork) = forked(&c, ForkMode::Copy);

        let (back, state) = reload(&holding(c, state));
        assert_eq!(
            state.seeding().iter().copied().collect::<Vec<_>>(),
            vec![fork],
            "a reload that lost the seed in flight leaves the fork provisioning for ever"
        );
        assert_eq!(
            back.agent, source,
            "a reload that forgot whose log this is would branch from nowhere"
        );
    }
}
