//! The session's conversation, and its forks. One struct for both, because a
//! fork *is* a conversation — one that carries a branch point.
//!
//! That is what collapses the five fork-shaped events the previous shape
//! needed — created, seeded, titled, status-changed, turn-ended — into the
//! vocabulary below. They existed only because a fork moved a roster entry
//! while the main agent moved the session's status; once every runner carries
//! its own status there is nothing left to tell apart, and a fork of a fork
//! stops being a case at all.
//!
//! [`Runner::outcome`] is always `None`, in every state including the terminal
//! ones. A conversation owes nobody a result, root or fork, which is what lets
//! a `parent` mean provenance rather than debt — and it makes "a fork reports
//! nothing" a property of this function rather than a check somewhere else that
//! a reader has to go and find.
//!
//! A fork does not run until its seed lands. The branch point is a copy or a
//! summary of the source's log, and an agent started before it exists would
//! answer from an empty transcript — so until [`State::seeded`] is true what
//! [`Runner::actions`] asks for is the branch point itself, and only then the
//! agent. A seed that failed leaves `seeded` false for good, and the fork asks
//! for nothing ever again.
//!
//! It is nonetheless *addressable* the whole time. [`State::agent`] is decided
//! when the fork is created, because the reply to `/fork` and the fork's row in
//! the session list both name it, and both are answered before the copy has
//! landed. Whether that agent is running is [`State::started`] — the question
//! the id used to answer by being `None`, which it can no longer do.

use super::action::{Branch, FirstInput};
use super::message::ChildOutcome;
use super::{Action, AgentId, AgentLifecycle, Emit, Runner, RunnerEvent, SessionView, TurnEnd};
use crate::agent_loop::UsageTotal;
use crate::agent_loop::capabilities::Capabilities;
use crate::sessions::session_actor::{AgentEntry, AgentStatus};
use crate::sessions::spec::{AgentSettings, SessionStatus};
use crate::sessions::supervisor::ForkRow;
use serde::{Deserialize, Serialize};

/// Where this conversation's turn is. The runner's own word for it, so the
/// session's status is a read of the root runner rather than a second variable
/// that can disagree with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TurnStatus {
    /// Between turns: waiting for a person, and nothing in flight.
    #[default]
    Idle,
    Running,
    /// The agent asked, and the turn is parked on the answer.
    AwaitingInput,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// The conversation's agent, known from the moment the conversation was
    /// created rather than from the moment its agent starts.
    ///
    /// A fork has to be addressable before it has said anything: the
    /// `MessageAccepted` that answers `/fork` names it, and so does the fork's
    /// row in the session list. Minted at start time, both would be answering
    /// with an id that does not exist yet.
    pub agent: AgentId,
    /// `None` is the session's own conversation; `Some` is a fork, and names
    /// where it branched from.
    pub seed: Option<Branch>,
    /// Whether that branch point has landed. Separate from `seed` because a
    /// fork exists — and is listed, and is titled — from the moment it is
    /// created, and only becomes runnable later.
    pub seeded: bool,
    /// Whether [`Self::agent`] has been started.
    ///
    /// The gate [`Runner::actions`] reads, and the one fact standing between a
    /// recovery and a double start. It used to be `agent.is_some()`; now that
    /// the id exists from creation, the id can no longer answer the question
    /// and this does.
    pub started: bool,
    pub turn: TurnStatus,
    /// What this conversation is called.
    ///
    /// The name it was branched as until something renames it. A *fork* is
    /// renamed here, by [`Event::Titled`]: `set_session_title` asks the
    /// session, and the session hands the name to the conversation the asking
    /// agent belongs to. The session's own conversation is not — it *is* the
    /// session, so naming it is a rename of the session, and a copy here would
    /// be a second writer of one fact.
    pub title: Option<String>,
    /// What a fork was told to do, and the first thing its agent is handed.
    /// `None` for the session's own conversation, which waits for a person.
    pub first_message: Option<String>,
    /// What this conversation runs under, fixed when it was created.
    pub settings: AgentSettings,
    /// This conversation's own tokens; the session's aggregate is by model.
    pub usage: UsageTotal,
    /// Why the last turn failed, or `None` if the last one did not.
    ///
    /// The session's `Failed` status has no other source: the error used to be
    /// carried into [`Event::TurnFailed`] and dropped on the floor by the fold,
    /// so a person was shown a conversation that had failed and nothing at all
    /// about why. Cleared when the next turn begins, because by then it is the
    /// previous attempt's reason and showing it beside a running turn is worse
    /// than showing nothing.
    pub last_error: Option<String>,
    /// When this conversation last did anything, from the entry that recorded
    /// it. A fork's row in the session list is ordered and stamped by it, and
    /// there is nowhere else to read it from — the session's own journal
    /// position is the whole session's, not this conversation's.
    pub last_activity_ms: u64,
    pub capabilities: Capabilities,
}

/// Not derived: [`AgentSettings`] has no `Default`. A live conversation's
/// settings arrive with its args; this is the empty slice, and nothing else
/// builds one.
#[cfg(test)]
impl Default for State {
    fn default() -> Self {
        Self {
            agent: AgentId::default(),
            seed: None,
            seeded: false,
            started: false,
            turn: TurnStatus::default(),
            title: None,
            first_message: None,
            settings: super::empty_settings(),
            usage: UsageTotal::default(),
            last_error: None,
            last_activity_ms: 0,
            capabilities: Capabilities::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// The agent this conversation was created with is now running.
    ///
    /// Carries no id: the id is [`State::agent`], written once at creation, and
    /// an event that repeated it would be a second writer of one field.
    Started,
    /// The branch point landed; a fork is now runnable like any other
    /// conversation.
    Seeded,
    SeedFailed {
        error: String,
    },
    TurnBegan,
    /// The agent asked and is parked on the answer.
    Asked,
    TurnEnded,
    TurnFailed {
        error: String,
    },
    /// This conversation was given a name.
    ///
    /// Only a fork records one. `set_session_title` names the conversation the
    /// asking agent is in, and for the session's own that is the session — so
    /// its rename is a `Renamed` on the session and never reaches here.
    Titled {
        name: String,
    },
    /// A person stopped the turn.
    TurnStopped,
    /// The process died inside the turn, and the agent said so at recovery.
    TurnInterrupted,
}

impl State {
    /// Whether this conversation is a fork whose branch point has still to
    /// land.
    ///
    /// One question asked in four places — the status a reader sees, what
    /// [`Runner::actions`] asks for, whether the session may unload, and
    /// whether the fork may start — so that they cannot answer it differently.
    ///
    /// A failed seed is *not* seeding. Nothing will retry it, so a fork that
    /// answered `true` here would ask to be seeded at every boundary for ever
    /// and hold the session loaded while it did.
    fn seeding(&self) -> bool {
        self.seed.is_some() && !self.seeded && self.turn != TurnStatus::Failed
    }

    /// Where this conversation's agent is, as a reader sees it.
    ///
    /// A fork is listed and addressable from the moment it is created, and
    /// becomes runnable only when its branch point lands — so it waits rather
    /// than rests. Unless the seed itself failed, which is a failure and not a
    /// wait: reported as `Provisioning` it would spin in the session list for
    /// ever.
    ///
    /// One source for both the roster entry and the fork's row in the session
    /// list, so the same conversation cannot be badged two ways.
    fn agent_status(&self) -> AgentStatus {
        let seeding = self.seeding();
        match self.turn {
            TurnStatus::Failed => AgentStatus::Failed,
            TurnStatus::Idle if seeding => AgentStatus::Provisioning,
            TurnStatus::Idle => AgentStatus::Idle,
            TurnStatus::Running => AgentStatus::Running,
            TurnStatus::AwaitingInput => AgentStatus::AwaitingInput,
        }
    }
}

impl Runner for State {
    fn started_event(&self) -> Option<RunnerEvent> {
        Some(RunnerEvent::Conversation(Event::Started))
    }

    fn actions(&self, _view: &SessionView) -> Vec<Action> {
        // A fork whose branch point has not landed is a conversation with no
        // transcript. Starting its agent here would have the model answer from
        // nothing, and the copy would then land underneath it — so the branch
        // point is what it asks for, and it goes on asking until the seed
        // lands or fails for good.
        //
        // Asked at every boundary rather than once, like every other action
        // here: a seed the last process was carrying when it died is
        // indistinguishable from one nobody has started, and asking again is
        // what makes those two the same case. The session recognises a seed it
        // already has in flight.
        //
        // Cloned rather than borrowed: an action is a value the session carries
        // off this state and performs later.
        if let Some(branch) = self.seed.clone().filter(|_| !self.seeded) {
            // A seed that failed is never retried, so this fork asks for
            // nothing ever again — the alternative is starting its agent
            // against the empty transcript the failure was about.
            return match self.seeding() {
                true => vec![Action::Seed {
                    fork: self.agent,
                    branch,
                }],
                false => Vec::new(),
            };
        }
        if self.started {
            return Vec::new();
        }
        vec![Action::StartAgent {
            // Read, never minted: this id was decided when the conversation was
            // created, so recovery asks to start the same agent the log already
            // named rather than a fresh one nothing can find.
            agent: self.agent,
            // A fresh copy for the agent's task to equip itself from; the
            // folded one stays here. The clone goes through the persisted
            // form, so the two cannot diverge from what a reload would build.
            equipment: self.capabilities.clone(),
            settings: Box::new(self.settings.clone()),
            // A conversation is nobody's typed worker.
            agent_type: None,
            first: match &self.first_message {
                Some(text) => FirstInput::Text(text.clone()),
                None => FirstInput::None,
            },
        }]
    }

    /// Always `None`, in every state including the terminal ones.
    ///
    /// A conversation owes nobody a result. Its `parent` records which agent
    /// branched it and nothing more, so a fork that failed is that fork's own
    /// status rather than a report the source is still waiting for.
    fn outcome(&self) -> Option<ChildOutcome> {
        None
    }

    /// A turn in flight, or a branch point still being built.
    ///
    /// Seeding counts because a summary is provider time with nothing durable
    /// behind it: unloaded halfway, the session stops the source's turn and the
    /// fork sits `Provisioning` until somebody happens to open it again. A
    /// failed seed does not count — see [`State::seeding`] — or a fork whose
    /// copy never landed would hold the session loaded for the rest of its
    /// life.
    fn busy(&self) -> bool {
        matches!(self.turn, TurnStatus::Running) || self.seeding()
    }

    /// Always `None`, for the same reason [`Runner::outcome`] is.
    ///
    /// A conversation is never over. A failed turn is a failed *turn* — the
    /// next thing a person types starts another one — so a conversation that
    /// reported `Failed` here would be marked terminal by the session and stop
    /// being handed anything, which is a session that has to be forked to be
    /// spoken to again.
    fn finished(&self) -> Option<super::RunnerStatus> {
        None
    }

    fn capabilities(&self) -> Option<&Capabilities> {
        Some(&self.capabilities)
    }

    /// One agent, which the read side badges with the session's own status
    /// when this conversation is the root.
    fn rows(&self) -> Vec<AgentEntry> {
        vec![AgentEntry {
            id: self.agent.to_string(),
            // Where I sit is the session's fact about me, not mine.
            parent: None,
            depth: 0,
            // Not one of several, so nothing to tell it apart by.
            label: None,
            agent_type: None,
            status: self.agent_status(),
            error: self.last_error.clone(),
            // My agent is as old as I am, so the read side stamps it from my
            // record rather than from a second copy kept here.
            started_at_ms: 0,
            ended_at_ms: 0,
        }]
    }

    /// The session's status is a read of this, which is why `turn` is the
    /// runner's own word for where it is rather than a second variable beside
    /// the session's.
    fn standing(&self) -> Option<SessionStatus> {
        Some(match self.turn {
            TurnStatus::Idle => SessionStatus::Idle,
            TurnStatus::Running => SessionStatus::Running,
            TurnStatus::AwaitingInput => SessionStatus::AwaitingInput,
            TurnStatus::Failed => SessionStatus::Failed {
                reason: self.last_error.clone().unwrap_or_default(),
            },
        })
    }

    fn primary_agent(&self) -> Option<AgentId> {
        Some(self.agent)
    }

    /// A fork is a conversation a person opens in its own right; the session's
    /// own is the session, and is already listed as one.
    fn listing(&self) -> Option<ForkRow> {
        // No branch point, no row: this conversation is the session.
        self.seed.as_ref()?;
        Some(ForkRow {
            id: self.agent.as_uuid(),
            // The read side's two: where I sit, and when I was created.
            parent: None,
            created_at_ms: 0,
            title: self.title.clone(),
            status: self.agent_status(),
            last_activity_ms: self.last_activity_ms,
        })
    }

    fn settings(&self, _agent: AgentId) -> Option<&AgentSettings> {
        Some(&self.settings)
    }

    // `task_and_output` keeps the trait's `(None, None)`: a conversation is
    // asked things one turn at a time, and what it said is its transcript
    // rather than a result.

    /// One total: a conversation owns one agent.
    fn usage(&self) -> Vec<(AgentId, UsageTotal)> {
        vec![(self.agent, self.usage)]
    }

    fn apply(&mut self, event: &RunnerEvent, at_ms: u64) {
        // Banking is the same act for every runner, so the session decides it
        // and each runner only chooses where to add it up. Here that is one
        // total: a conversation owns one agent.
        if let RunnerEvent::Usage { spent, .. } = event {
            self.usage = self.usage.combine(spent);
            return;
        }
        // Every other arm belongs to another runner, or is a capability's own
        // event that `RunnerState::apply` has already routed.
        let RunnerEvent::Conversation(event) = event else {
            return;
        };
        // Every one of its own events counts as activity, before the match
        // rather than in nine arms: a fork's row in the session list is
        // ordered by this, and an arm somebody forgot would make a busy
        // conversation read as the oldest one there.
        self.last_activity_ms = at_ms;
        match event {
            Event::Started => self.started = true,
            Event::Seeded => self.seeded = true,
            // `seeded` stays false, so this fork never starts an agent: a
            // conversation with no transcript has nothing to continue.
            Event::SeedFailed { error } => {
                self.turn = TurnStatus::Failed;
                self.last_error = Some(error.clone());
            }
            Event::TurnBegan => {
                self.turn = TurnStatus::Running;
                // The previous attempt's reason, and this one is under way:
                // shown beside a running turn it reads as a live failure.
                self.last_error = None;
            }
            Event::Titled { name } => self.title = Some(name.clone()),
            Event::Asked => self.turn = TurnStatus::AwaitingInput,
            Event::TurnEnded | Event::TurnStopped | Event::TurnInterrupted => {
                self.turn = TurnStatus::Idle;
            }
            Event::TurnFailed { error } => {
                self.turn = TurnStatus::Failed;
                self.last_error = Some(error.clone());
            }
        }
    }
}

impl AgentLifecycle for State {
    fn on_agent_started(&self, _agent: AgentId) -> Emit {
        Emit::record(vec![RunnerEvent::Conversation(Event::TurnBegan)])
    }

    fn on_agent_ended(&self, _agent: AgentId, end: &TurnEnd) -> Emit {
        let event = match end {
            // The output is the agent's own transcript, which the session
            // never copies: what ended is the turn, and that is all this
            // records.
            TurnEnd::Concluded { .. } => Event::TurnEnded,
            TurnEnd::Asked => Event::Asked,
            // `terminal` is not read here either: a conversation that failed
            // is failed, and whether the sandbox can come back is the runtime
            // runner's fact to hold.
            TurnEnd::Failed { error, .. } => Event::TurnFailed {
                error: error.clone(),
            },
            TurnEnd::Parked => Event::TurnFailed {
                error: "timers are not supported in sessions".to_string(),
            },
            // Only while a turn is actually running. A report about a turn
            // that already ended is history that has been written once
            // already, and rewriting it would move an idle conversation
            // backwards at every recovery.
            TurnEnd::Interrupted => {
                if self.turn != TurnStatus::Running {
                    return Emit::nothing();
                }
                Event::TurnInterrupted
            }
        };
        Emit::record(vec![RunnerEvent::Conversation(event)])
    }

    fn on_agent_halted(&self, _agent: AgentId, reason: &str) -> Emit {
        Emit::record(vec![RunnerEvent::Conversation(Event::TurnFailed {
            error: reason.to_string(),
        })])
    }

    /// Only while a turn is actually running.
    ///
    /// The gate is `Running` and deliberately not also `AwaitingInput`:
    /// stopping does not clear the questions the agent is parked on, so a
    /// boundary journaled over a park would read `Idle` beside questions still
    /// waiting for an answer.
    fn on_agent_stopped(&self, _agent: AgentId) -> Emit {
        if self.turn != TurnStatus::Running {
            return Emit::nothing();
        }
        Emit::record(vec![RunnerEvent::Conversation(Event::TurnStopped)])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::Capability;
    use crate::agent_loop::capabilities::testing::{advertised, facts};
    use crate::agent_loop::capabilities::title::TitleCapability;
    use crate::sessions::forks::ForkMode;

    fn view() -> SessionView {
        SessionView {
            runtime_ready: true,
            depth: 0,
            active_agents: 0,
        }
    }

    fn fork() -> State {
        State {
            agent: AgentId::new_v4(),
            seed: Some(Branch {
                source: AgentId::new_v4(),
                source_seq: 0,
                mode: ForkMode::Copy,
            }),
            first_message: Some("carry on elsewhere".into()),
            ..State::default()
        }
    }

    fn ended(state: &State, end: &TurnEnd) -> Vec<RunnerEvent> {
        state.on_agent_ended(AgentId::new_v4(), end).events
    }

    fn only_event(events: Vec<RunnerEvent>) -> Event {
        assert_eq!(events.len(), 1, "expected one event, got {events:?}");
        let RunnerEvent::Conversation(event) = &events[0] else {
            panic!("expected a conversation event, got {:?}", events[0]);
        };
        event.clone()
    }

    fn started() -> RunnerEvent {
        RunnerEvent::Conversation(Event::Started)
    }

    /// A conversation that has not started its agent starts it, and one that
    /// has starts nothing — the same pure read that lets creation and recovery
    /// share a path. Its first input is `None`: the session's own conversation
    /// waits for a person rather than being handed something to answer.
    #[test]
    fn a_conversation_with_no_agent_starts_one_and_waits_for_a_person() {
        let mut state = State {
            agent: AgentId::new_v4(),
            ..State::default()
        };
        let actions = state.actions(&view());
        let Action::StartAgent { agent, first, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        // The id this conversation was created with, not a fresh one: it is
        // already what everything outside this runner addresses.
        assert_eq!(*agent, state.agent);
        assert!(matches!(first, FirstInput::None));
        assert_eq!(actions.len(), 1);

        state.apply(&started(), 0);
        assert!(state.actions(&view()).is_empty());
    }

    /// **A fork asks for its branch point, and goes on asking.**
    ///
    /// The one action a conversation has before it has an agent. It is asked
    /// for at every boundary rather than once, because nothing journals that a
    /// seed is in flight — which is exactly what makes a seed the last process
    /// abandoned indistinguishable from one nobody has started, and so
    /// repairable. The session recognises one it already has going.
    #[test]
    fn a_fork_asks_to_be_seeded_at_every_boundary_until_its_seed_lands() {
        let state = fork();
        for _ in 0..2 {
            let actions = state.actions(&view());
            assert_eq!(actions.len(), 1, "expected one action, got {actions:?}");
            let Action::Seed { fork, branch } = &actions[0] else {
                panic!("expected a seed, got {:?}", actions[0]);
            };
            assert_eq!(*fork, state.agent, "the seed names the wrong conversation");
            assert_eq!(
                Some(branch),
                state.seed.as_ref(),
                "the branch point asked for is not the one recorded"
            );
        }

        // And once it lands there is nothing left to ask for: the agent is.
        let mut seeded = state;
        seeded.apply(&RunnerEvent::Conversation(Event::Seeded), 0);
        assert!(matches!(
            seeded.actions(&view())[0],
            Action::StartAgent { .. }
        ));
    }

    /// The session's own conversation has nothing to wait for, so it never asks
    /// for a branch point at all.
    #[test]
    fn a_conversation_that_is_not_a_fork_never_asks_for_a_seed() {
        let actions = State::default().actions(&view());
        assert!(matches!(actions[0], Action::StartAgent { .. }));
        assert_eq!(actions.len(), 1);
    }

    /// **A seed in flight holds the session open.** A summary is provider time
    /// with nothing durable behind it: unloaded halfway, the source's turn is
    /// stopped and the fork sits `Provisioning` until somebody happens to open
    /// it again.
    ///
    /// And a *failed* seed does not, or a fork whose copy never landed would
    /// keep the session loaded for the rest of its life.
    #[test]
    fn a_seed_in_flight_is_busy_and_a_failed_one_is_not() {
        let mut state = fork();
        assert!(state.busy(), "a branch point being built is work in flight");

        let mut failed = state.clone();
        failed.apply(
            &RunnerEvent::Conversation(Event::SeedFailed {
                error: "the copy failed".into(),
            }),
            0,
        );
        assert!(
            !failed.busy(),
            "nothing will retry it, so nothing is waiting"
        );
        assert!(failed.actions(&view()).is_empty());

        state.apply(&RunnerEvent::Conversation(Event::Seeded), 0);
        assert!(!state.busy(), "a fork between turns is idle like any other");
    }

    /// **A fork is addressable before it can run.** The `MessageAccepted` that
    /// answers `/fork` names the new conversation's agent, and so does its row
    /// in the session list — both of which are answered while the copy of the
    /// source's log is still in flight. Minted when the agent starts instead,
    /// there would be nothing to name, and the id the person was given would
    /// belong to no agent at all.
    #[test]
    fn a_fork_has_an_addressable_agent_before_its_seed_lands() {
        let mut state = fork();
        let addressed = state.agent;
        assert!(
            !state
                .actions(&view())
                .iter()
                .any(|a| matches!(a, Action::StartAgent { .. })),
            "it must not run before its seed lands"
        );

        state.apply(&RunnerEvent::Conversation(Event::Seeded), 0);
        let actions = state.actions(&view());
        let Action::StartAgent { agent, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        assert_eq!(
            *agent, addressed,
            "the fork started an agent nobody had been told about"
        );
    }

    /// **A fork must not run before its branch point is durable.** Started
    /// early, its agent answers from an empty transcript and the copy of the
    /// source's log then lands underneath it — the conversation the person
    /// asked to continue is gone, and nothing reports an error.
    #[test]
    fn a_fork_does_not_start_until_its_seed_lands() {
        let mut state = fork();
        assert!(matches!(state.actions(&view())[0], Action::Seed { .. }));

        state.apply(&RunnerEvent::Conversation(Event::Seeded), 0);
        let actions = state.actions(&view());
        assert_eq!(actions.len(), 1);
        let Action::StartAgent { first, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        let FirstInput::Text(text) = first else {
            panic!("expected the fork's message, got {first:?}");
        };
        assert_eq!(text, "carry on elsewhere");
    }

    /// A seed that failed leaves the fork unrunnable for good. If `SeedFailed`
    /// only recorded a status, the next boundary would start the agent anyway
    /// against the empty transcript the failure was about.
    #[test]
    fn a_fork_whose_seed_failed_never_starts() {
        let mut state = fork();
        state.apply(
            &RunnerEvent::Conversation(Event::SeedFailed {
                error: "the copy failed".into(),
            }),
            0,
        );
        assert_eq!(state.turn, TurnStatus::Failed);
        assert!(state.actions(&view()).is_empty());
    }

    /// Equipment comes from folding this conversation's capabilities, which is
    /// what replaces the per-kind toolbox match: a fork is equipped by holding
    /// different capabilities, not by being a different kind of agent.
    #[test]
    fn the_agent_is_equipped_by_folding_the_capabilities() {
        let state = State {
            capabilities: Capabilities::new(vec![Capability::Title(TitleCapability::default())]),
            ..State::default()
        };
        let actions = state.actions(&view());
        let Action::StartAgent { equipment, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        assert_eq!(
            advertised(equipment, &facts()),
            vec![crate::agent_loop::capabilities::title::TOOL],
            "the capability it holds is what its agent runs with"
        );
    }

    /// **A conversation never reports an outcome, in any state.** This is what
    /// lets `parent` mean provenance rather than debt: give a fork an outcome
    /// and the agent that typed `/fork` acquires an outstanding child it must
    /// wait for, and a conversation that ends becomes a report somebody is owed.
    #[test]
    fn a_conversation_never_reports_an_outcome() {
        let mut state = fork();
        assert!(state.outcome().is_none());
        for event in [
            Event::Seeded,
            Event::Started,
            Event::TurnBegan,
            Event::Asked,
            Event::TurnEnded,
            Event::TurnStopped,
            Event::TurnInterrupted,
            Event::TurnFailed {
                error: "it broke".into(),
            },
            Event::SeedFailed {
                error: "the copy failed".into(),
            },
        ] {
            state.apply(&RunnerEvent::Conversation(event), 0);
            assert!(
                state.outcome().is_none(),
                "a conversation reported an outcome from {:?}",
                state.turn
            );
        }
    }

    /// Busy is "a turn is in flight". A conversation parked on a question must
    /// not read busy — that is exactly the state a session is offloaded in, and
    /// a person may not answer for hours.
    #[test]
    fn it_is_busy_only_while_a_turn_runs() {
        let mut state = State::default();
        assert!(!state.busy());
        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 0);
        assert!(state.busy());
        state.apply(&RunnerEvent::Conversation(Event::Asked), 0);
        assert!(!state.busy());
        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 0);
        assert!(state.busy());
        state.apply(&RunnerEvent::Conversation(Event::TurnEnded), 0);
        assert!(!state.busy());
    }

    /// Only its own events touch its slice. Folding a neighbour's event here
    /// would let one runner's log rewrite another's state on replay.
    #[test]
    fn another_runners_event_is_a_no_op() {
        let mut state = State::default();
        state.apply(
            &RunnerEvent::SubAgent(crate::sessions::runners::subagent::Event::Failed {
                error: "not mine".into(),
            }),
            0,
        );
        assert_eq!(state.turn, TurnStatus::Idle);
        assert!(!state.started);
    }

    /// Starting an agent is the start of a turn. Without this the conversation
    /// would read idle while the model was mid-answer, and the session could be
    /// offloaded underneath it.
    #[test]
    fn starting_an_agent_begins_a_turn() {
        let emit = State::default().on_agent_started(AgentId::new_v4());
        assert!(emit.actions.is_empty());
        assert!(matches!(only_event(emit.events), Event::TurnBegan));
    }

    /// A conclusion ends the turn and nothing more: what the agent said lives
    /// in the agent's own journal, and copying it here would be a second copy
    /// to keep in step.
    #[test]
    fn a_concluded_turn_ends_the_turn() {
        let event = only_event(ended(
            &State::default(),
            &TurnEnd::Concluded {
                output: serde_json::json!("done"),
            },
        ));
        assert!(matches!(event, Event::TurnEnded));
    }

    /// An ask parks the turn rather than ending it. Read as an ending, the
    /// conversation would go idle while an unanswered question sat in front of
    /// the person, and nothing would show it as waiting.
    #[test]
    fn an_ask_parks_the_turn() {
        let mut state = State::default();
        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 0);
        let event = only_event(ended(&state, &TurnEnd::Asked));
        assert!(matches!(event, Event::Asked));
        state.apply(&RunnerEvent::Conversation(event), 0);
        assert_eq!(state.turn, TurnStatus::AwaitingInput);
    }

    /// A failing turn carries its error through verbatim, terminal or not: it
    /// is the only thing a person will be shown about why the turn stopped.
    #[test]
    fn a_failing_turn_fails_the_turn_with_its_error() {
        for terminal in [true, false] {
            let event = only_event(ended(
                &State::default(),
                &TurnEnd::Failed {
                    error: "the model refused".into(),
                    terminal,
                },
            ));
            let Event::TurnFailed { error } = event else {
                panic!("expected a failed turn, got {event:?}");
            };
            assert_eq!(error, "the model refused");
        }
    }

    /// A conversation has no timer tools, so a park is a turn stopped for
    /// something that will never arrive. Failing it in words is what stops the
    /// person waiting on a conversation that reads as running.
    #[test]
    fn a_park_fails_the_turn_because_sessions_have_no_timers() {
        let event = only_event(ended(&State::default(), &TurnEnd::Parked));
        let Event::TurnFailed { error } = event else {
            panic!("expected a failed turn, got {event:?}");
        };
        assert!(error.contains("timers"));
    }

    /// An interruption counts only against a turn that was running. A report
    /// arriving about a turn that already ended is history already written, and
    /// acting on it would move an idle conversation backwards at every
    /// recovery.
    #[test]
    fn an_interruption_counts_only_during_a_running_turn() {
        let mut state = State::default();
        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 0);
        let event = only_event(ended(&state, &TurnEnd::Interrupted));
        assert!(matches!(event, Event::TurnInterrupted));
        state.apply(&RunnerEvent::Conversation(event), 0);
        assert_eq!(state.turn, TurnStatus::Idle);

        // Idle, awaiting input and failed all mean the turn's ending was
        // recorded already.
        for turn in [
            TurnStatus::Idle,
            TurnStatus::AwaitingInput,
            TurnStatus::Failed,
        ] {
            let settled = State {
                turn,
                ..State::default()
            };
            let emit = settled.on_agent_ended(AgentId::new_v4(), &TurnEnd::Interrupted);
            assert!(emit.events.is_empty(), "{turn:?} emitted {:?}", emit.events);
            assert!(emit.actions.is_empty());
        }
    }

    /// A halted turn is a failed turn carrying the halting reason. Without it,
    /// a conversation stopped by a hook would read as running for ever.
    #[test]
    fn a_halt_fails_the_turn_with_its_reason() {
        let emit = State::default().on_agent_halted(AgentId::new_v4(), "blocked by a hook");
        assert!(emit.actions.is_empty());
        let Event::TurnFailed { error } = only_event(emit.events) else {
            panic!("expected a failed turn");
        };
        assert_eq!(error, "blocked by a hook");
    }

    /// A stop is a turn that ended, not one that failed: the person asked for
    /// it, and the next thing they do is type again.
    #[test]
    fn a_stopped_turn_goes_back_to_idle() {
        let mut state = State::default();
        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 0);
        state.apply(&RunnerEvent::Conversation(Event::TurnStopped), 0);
        assert_eq!(state.turn, TurnStatus::Idle);
    }

    /// Stopping counts only against a turn that was actually running.
    ///
    /// A person can press stop while the ending is already in flight, so this
    /// arrives over a settled turn routinely — and a boundary written then
    /// moves the conversation backwards. `AwaitingInput` is the arm worth
    /// naming: stopping does not clear the questions the agent is parked on, so
    /// a stop recorded there would read `Idle` beside questions still waiting.
    #[test]
    fn stopping_counts_only_against_a_running_turn() {
        let mut state = State::default();
        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 0);
        let emit = state.on_agent_stopped(state.agent);
        assert!(emit.actions.is_empty());
        assert!(matches!(only_event(emit.events), Event::TurnStopped));

        for turn in [
            TurnStatus::Idle,
            TurnStatus::AwaitingInput,
            TurnStatus::Failed,
        ] {
            let settled = State {
                turn,
                ..State::default()
            };
            let emit = settled.on_agent_stopped(settled.agent);
            assert!(emit.events.is_empty(), "{turn:?} emitted {:?}", emit.events);
            assert!(emit.actions.is_empty());
        }
    }

    /// **The session's `Failed` status has no other source.** The error used to
    /// reach `TurnFailed` and be dropped by the fold, so a person saw a
    /// conversation that had failed and nothing at all about why. And the next
    /// turn clears it: kept, it would sit beside a running turn reading as a
    /// live failure.
    #[test]
    fn a_failed_turn_keeps_the_reason_the_session_reports() {
        let mut state = State::default();
        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 0);
        assert_eq!(state.last_error, None);

        state.apply(
            &RunnerEvent::Conversation(Event::TurnFailed {
                error: "the model refused".into(),
            }),
            0,
        );
        assert_eq!(state.turn, TurnStatus::Failed);
        assert_eq!(state.last_error.as_deref(), Some("the model refused"));

        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 0);
        assert_eq!(
            state.last_error, None,
            "the previous attempt's reason was shown beside a running turn"
        );
    }

    /// A fork's row in the session list is ordered and stamped by this, so it
    /// moves with the fork's own turns and reads the time off the entry that
    /// recorded them — never a clock, or a replay would restamp the whole list
    /// with the moment of recovery.
    #[test]
    fn a_forks_last_activity_moves_with_its_turns() {
        let mut state = fork();
        assert_eq!(state.last_activity_ms, 0);

        state.apply(&RunnerEvent::Conversation(Event::Seeded), 100);
        assert_eq!(state.last_activity_ms, 100);
        state.apply(&RunnerEvent::Conversation(Event::TurnBegan), 250);
        assert_eq!(state.last_activity_ms, 250);
        state.apply(&RunnerEvent::Conversation(Event::TurnEnded), 900);
        assert_eq!(state.last_activity_ms, 900);

        // Another runner's event is not this fork's activity.
        state.apply(
            &RunnerEvent::SubAgent(crate::sessions::runners::subagent::Event::Started),
            5_000,
        );
        assert_eq!(state.last_activity_ms, 900);
    }

    /// A conversation owns one agent, so its tokens are one total.
    #[test]
    fn banked_tokens_land_on_the_conversations_own_total() {
        let mut state = State::default();
        let spent = UsageTotal {
            input_tokens: 10,
            output_tokens: 4,
            ..Default::default()
        };
        for _ in 0..2 {
            state.apply(
                &RunnerEvent::Usage {
                    agent: state.agent,
                    model: "sonnet".into(),
                    spent,
                },
                0,
            );
        }
        assert_eq!(state.usage.input_tokens, 20);
        assert_eq!(state.usage.output_tokens, 8);
    }

    /// **A conversation is never over.** A failed turn is a failed turn; the
    /// next thing a person types starts another one. Report a status here and
    /// the session marks the runner terminal and stops handing it anything —
    /// a conversation that has to be forked to be spoken to again.
    #[test]
    fn a_conversation_never_finishes() {
        let mut state = fork();
        for event in [
            Event::Seeded,
            Event::Started,
            Event::TurnBegan,
            Event::Asked,
            Event::TurnEnded,
            Event::TurnStopped,
            Event::TurnInterrupted,
            Event::TurnFailed {
                error: "it broke".into(),
            },
            Event::SeedFailed {
                error: "the copy failed".into(),
            },
        ] {
            state.apply(&RunnerEvent::Conversation(event), 0);
            assert!(
                state.finished().is_none(),
                "a conversation finished from {:?}",
                state.turn
            );
        }
    }
}
