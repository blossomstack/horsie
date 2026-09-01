//! The contract every component of an agent shares — and everything they are
//! allowed to share.
//!
//! A component is an instantiated struct the actor holds. It owns its own
//! in-memory bookkeeping (the turn in flight, a prepare step's flag) and the
//! commands routed to it; the actor is a router and keeps no domain logic.
//! Three things are shared, and nothing else:
//!
//! - **[`AgentState`]** — the durable state, moved only by the fold.
//! - **The command/event vocabulary** — a component acts by returning events
//!   in a [`CommandEffect`] and by telling *commands* to the shared mailbox.
//!   Components never call each other: a queue that decides a turn may start
//!   tells `StartTurn`; a turn that reaches a boundary tells `Drain`. The
//!   mailbox orders those against the persists that precede them, which is
//!   what makes the hand-offs crash-safe without any direct coupling.
//! - **[`Scratch`]** — the transient half of the state: the few in-memory
//!   facts more than one component genuinely reads, deliberately unjournaled.
//!
//! `apply` folds one component's events into state and must be pure — no I/O,
//! no clock, no scratch. `on_load` repairs what a dead process left behind.

use super::*;
use async_trait::async_trait;
use horsie_actor::{ActorContext, CommandEffect};

/// The transient half of an agent's state: in-memory facts more than one
/// component reads. Everything here is rebuilt or re-decided on recovery,
/// which is exactly why none of it is journaled.
pub(super) struct Scratch {
    /// A turn is committed or in flight. Raised by the queue the moment it
    /// commits a drained turn — before the turn component has even seen it —
    /// and lowered by the turn at every boundary. This is the busy gate: the
    /// queue must not drain a second turn into the gap between the commit and
    /// the `StartTurn` command being handled.
    pub turn_live: bool,
    /// Whether this agent's session has a runtime to run on. Seeded at spawn
    /// and moved by the `Runtime` lifecycle records the owner already sends.
    pub ready: bool,
    /// What the next turn should tell the provider about tool use. Taken when
    /// a turn starts, so it applies to exactly one turn. Set only when
    /// re-running a turn that ended without the result it owed.
    pub pending_tool_choice: Option<horsie_agentcore::ToolChoice>,
    /// Chunks of the message currently being written, since the newest log
    /// entry. Cleared whenever an entry lands, because the entry supersedes
    /// them. Written by the turn, read by the read path.
    pub deltas: Vec<String>,
}

impl Scratch {
    pub fn new(ready: bool) -> Self {
        Self {
            turn_live: false,
            ready,
            pending_tool_choice: None,
            deltas: Vec::new(),
        }
    }
}

/// What a component is handed with every command: the durable state as it
/// stands, the shared scratch, the agent's configuration, and the means to
/// spawn work and reach its own mailbox. Nothing here belongs to another
/// component.
pub(super) struct Cx<'a> {
    pub state: &'a AgentState,
    pub scratch: &'a mut Scratch,
    pub runtime: &'a crate::agent_loop::context::AgentRuntimeContext,
    pub params: &'a AgentParams,
    pub actor: &'a ActorContext<AgentCommand>,
}

impl Cx<'_> {
    /// Announce that this agent has moved, waking every reader waiting on it.
    /// Announcing twice for one change is harmless.
    pub fn publish_revision(&self) {
        self.runtime.revision.send_modify(|r| *r += 1);
    }

    /// Put a command on this agent's own mailbox. It is handled after
    /// whatever the current handler persists is durable and folded — the
    /// ordering every cross-component hand-off relies on.
    pub async fn tell(&self, cmd: AgentCommand) {
        let _ = self.actor.self_ref().tell(cmd).await;
    }

    /// Ask the queue to reconsider whether a turn may start. The universal
    /// "something changed" signal: a boundary reached, a message accepted, a
    /// timer fired, the runtime arriving.
    pub async fn drain(&self) {
        self.tell(AgentCommand::Queue(QueueCommand::Drain)).await;
    }
}

/// The component registry: every component an agent runs, held and named in
/// exactly one place.
///
/// The actor holds one of these and delegates wholesale — it never names a
/// component. Adding a component means editing this struct and its three
/// exhaustive routings below, all in this file, all checked at compile time:
/// a new command group or event variant that is not routed fails to build
/// *here*, where it has to be classified.
///
/// This is also the seam for building an agent's components from its spec
/// later: construction is centralized in [`Components::new`], so a spec-driven
/// variant changes this file and nothing above it.
pub(super) struct Components {
    timers: Timers,
    turn: Turn,
    queue: Queue,
    reads: Reads,
    log: LogWrites,
    seed: Seeding,
}

impl Components {
    pub fn new() -> Self {
        Self {
            timers: Timers,
            turn: Turn::default(),
            queue: Queue::default(),
            reads: Reads,
            log: LogWrites,
            seed: Seeding,
        }
    }

    /// Route one command to the component that owns its group. Exhaustive:
    /// a command group added later fails to compile here.
    ///
    /// `Core` is deliberately absent — the actor's own lifetime is the one
    /// thing that is nobody's component.
    pub async fn handle(
        &mut self,
        cmd: AgentCommand,
        cx: &mut Cx<'_>,
    ) -> Option<CommandEffect<AgentDomainEvent>> {
        Some(match cmd {
            AgentCommand::Queue(c) => self.queue.handle(c, cx).await,
            AgentCommand::Run(c) => self.turn.handle(c, cx).await,
            AgentCommand::Timer(c) => self.timers.handle(c, cx).await,
            AgentCommand::Read(c) => self.reads.handle(c, cx).await,
            AgentCommand::Log(c) => self.log.handle(c, cx).await,
            AgentCommand::Seed(c) => self.seed.handle(c, cx).await,
            AgentCommand::Core(_) => return None,
        })
    }

    /// Ask each component, in registration order, to repair what a dead
    /// process left it holding. The queue drains last, so it sees whatever
    /// gate the turn's own repair raised.
    pub async fn on_load(&mut self, cx: &mut Cx<'_>) {
        self.timers.on_load(cx).await;
        self.turn.on_load(cx).await;
        self.queue.on_load(cx).await;
    }
}

/// Fold one event into state by routing it to the component that owns it.
///
/// The one shared state-transition function — live handling, replay and every
/// component's own fold-forward all go through here, so they cannot disagree.
/// Exhaustive on purpose: an event added later fails to compile *here*, where
/// it has to be classified, rather than silently reaching the wrong fold.
///
/// A free function rather than a method because a fold is pure and replay must
/// not depend on which components an agent was instantiated with: any journal
/// ever written stays readable, whatever a future spec chooses to run.
pub(super) fn fold(mut state: AgentState, event: AgentDomainEvent) -> AgentState {
    match event {
        e @ AgentDomainEvent::Seeded { .. } => Seeding::apply(&mut state, e),
        e @ (AgentDomainEvent::InputMessage { .. }
        | AgentDomainEvent::Received { .. }
        | AgentDomainEvent::TurnBegan { .. }
        | AgentDomainEvent::AskRecorded { .. }
        | AgentDomainEvent::Parked { .. }) => Queue::apply(&mut state, e),
        e @ (AgentDomainEvent::MessageComplete { .. }
        | AgentDomainEvent::MessageAborted { .. }
        | AgentDomainEvent::ToolComplete { .. }
        | AgentDomainEvent::RunComplete { .. }
        | AgentDomainEvent::RunAborted { .. }
        | AgentDomainEvent::RunCancelled { .. }
        | AgentDomainEvent::Nudged { .. }) => Turn::apply(&mut state, e),
        e @ (AgentDomainEvent::HookRan { .. } | AgentDomainEvent::LifecycleRecorded { .. }) => {
            LogWrites::apply(&mut state, e)
        }
        e @ AgentDomainEvent::Compacted { .. } => Compaction::apply(&mut state, e),
        e @ (AgentDomainEvent::TimerArmed { .. }
        | AgentDomainEvent::TimerCancelled { .. }
        | AgentDomainEvent::TimerFired { .. }) => Timers::apply(&mut state, e),
        e @ AgentDomainEvent::TaskListChanged { .. } => TaskLists::apply(&mut state, e),
    }
    state
}

/// Fold several events forward over a snapshot — what a handler does to see
/// the state its own events leave behind before deciding what comes next.
pub(super) fn fold_all(state: &AgentState, events: &[AgentDomainEvent]) -> AgentState {
    events.iter().cloned().fold(state.clone(), fold)
}

/// The shape every component shares. `handle` decides — state in, effect out;
/// the actor persists and folds. A component with no instance state still
/// implements this so the actor treats every field the same way.
#[async_trait]
pub(super) trait Component {
    /// The command group routed to this component.
    type Command;

    /// Decide what `cmd` means. Reads whatever it likes through `cx`, writes
    /// only by returning events (durable) or touching scratch (transient), and
    /// reaches other components only by telling commands.
    async fn handle(
        &mut self,
        cmd: Self::Command,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent>;

    /// Fold one of this component's events into state.
    ///
    /// Must be pure — no I/O, no clock, no scratch. An associated function
    /// rather than a method because replay happens before any component
    /// instance exists.
    fn apply(_state: &mut AgentState, _event: AgentDomainEvent) {}

    /// Repair whatever a dead process left this component holding, once
    /// recovery has finished and before the first live command is handled.
    /// Nothing here persists; anything that needs to journal arrives as an
    /// ordinary command.
    async fn on_load(&mut self, _cx: &mut Cx<'_>) {}
}
