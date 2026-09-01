//! One agent: its session, the turn it is running, and the log both leave
//! behind.
//!
//! An agent is event-sourced, so a restarted process recovers an in-flight
//! session from the journal and continues. Everything durable about it is
//! [`AgentState`], everything it can be told is [`AgentCommand`], and every
//! change is an [`AgentDomainEvent`] that was journaled before it was believed.
//!
//! The actor itself is a router and nothing more. Each command group belongs
//! to one **component** — an instantiated struct the actor holds — and the
//! actor's whole job is to hand the command over, persist whatever the
//! component decided, and keep the plumbing every component relies on (the
//! observer, the revision counter, the snapshot cadence). The components are
//! decoupled from the actor and from each other: what they share is the state,
//! the command/event vocabulary, and the transient scratch — see
//! [`component`] for the contract.
//!
//! The components: [`queue`] the promises this agent has accepted and the
//! decision to answer them, [`run`] the turn in flight (with [`conclude`] as
//! its other half: what an ending means), [`timers`] the wake-ups it armed,
//! [`reads`] the questions that wake nothing, [`log`] what others write into
//! its transcript, and [`seed`] branching. [`compaction`] and [`task_list`]
//! are apply-only modules: they own events and folds but no commands.
//!
//! Two things deliberately do not happen on this mailbox. No provider call and
//! no toolbox build: those run on a spawned task, so a thirty-second MCP
//! connect cannot block a cancel. And no decision about whether this agent
//! exists: residency belongs to whoever spawned it.

mod compaction;
mod component;
mod conclude;
mod log;
mod queue;
mod reads;
mod repair;
mod run;
mod seed;
mod state;
mod task_list;
#[cfg(test)]
pub(super) mod testing;
mod timers;
mod types;

pub use reads::{ReadOutcome, ReplayWindow};
pub use state::{AgentState, UsageTotal, hook_entry, hook_entry_id};
pub use types::*;

use compaction::{COMPACT_AT_PERCENT, COMPACT_RETAIN_PERCENT, Compaction};
use component::{Component, Cx, Scratch, fold, fold_all};
use log::LogWrites;
use queue::Queue;
use reads::Reads;
use repair::{missing_tool_results, parked_call_ids, repair_unanswered_tool_calls};
use run::{RunOutcome, RunReport, SeedSummary, StepReport, Turn};
use seed::Seeding;
use state::new_message_id;
use task_list::TaskLists;
use timers::Timers;

use crate::agent_loop::context::AgentRuntimeContext;
use async_trait::async_trait;
use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor, PersistenceId};
use std::sync::Arc;

/// Events an agent may journal between snapshots before the next one is taken.
///
/// An agent's state *is* its transcript, so a snapshot costs O(transcript) to
/// write — snapshotting every turn would be quadratic over a session. This
/// trades a bounded replay on recovery for a bounded write amplification.
const SNAPSHOT_EVERY_EVENTS: u64 = 200;

/// Whether folding this event appends a log entry — i.e. consumes a `seq`.
///
/// Kept beside the fold deliberately: the two must agree, and a variant added
/// to one without the other would either strand deltas under an entry that
/// superseded them or clear them for an event that appended nothing.
fn coarse_appends_an_entry(e: &AgentDomainEvent) -> bool {
    matches!(
        e,
        AgentDomainEvent::InputMessage { .. }
            | AgentDomainEvent::MessageComplete { .. }
            | AgentDomainEvent::MessageAborted { .. }
            | AgentDomainEvent::ToolComplete { .. }
            | AgentDomainEvent::HookRan { .. }
            | AgentDomainEvent::LifecycleRecorded { .. }
            | AgentDomainEvent::TaskListChanged { .. }
    )
}

/// Observer of an agent's durable history, notified once per event that is both
/// journaled and folded into state.
///
/// This is how a live stream learns what happened without reading the journal:
/// the actor is the only thing that touches its own log, and this is the seam
/// it publishes through. Implementations must not block — they run on the
/// actor's mailbox — and must treat delivery as best-effort.
pub trait AgentObserver: Send + Sync {
    /// `state` is the state *after* `event` was folded, so an observer that
    /// needs the resulting message can read the log's tail rather than
    /// re-deriving it from the event.
    fn publish(&self, event: &AgentDomainEvent, state: &AgentState);
}

/// An agent, modelled as an event-sourced actor over components.
///
/// Everything domain-shaped lives in a component; what stays here is the
/// routing and the plumbing every component relies on.
pub struct AgentActor {
    runtime: AgentRuntimeContext,
    params: AgentParams,
    /// The transient half of the state — see [`component::Scratch`].
    scratch: Scratch,
    // The components, one per command group.
    queue: Queue,
    turn: Turn,
    timers: Timers,
    reads: Reads,
    log: LogWrites,
    seed: Seeding,
    /// Where durable history is published, when anyone is listening. `None` for
    /// workflow agents, which have no live stream.
    observer: Option<Arc<dyn AgentObserver>>,
    /// Events journaled since a snapshot was last *requested*. Counting
    /// requests rather than confirmed writes means a failed snapshot simply
    /// waits another interval, which is the right instinct for an
    /// optimization: retrying hard against a failing journal helps nobody.
    events_since_snapshot: u64,
    /// A counter, bumped whenever this agent moves, for readers to wait on.
    /// Held behind an `Arc` because the *owner* is whoever outlives this actor
    /// — for a session agent that is the supervisor, so an idle offload does
    /// not disconnect a reader.
    revision: std::sync::Arc<tokio::sync::watch::Sender<crate::sessions::Revision>>,
}

impl AgentActor {
    pub fn new(ctx: AgentRuntimeContext, params: AgentParams) -> Self {
        let revision = ctx.revision.clone();
        let scratch = Scratch::new(ctx.ready);
        Self {
            runtime: ctx,
            params,
            scratch,
            queue: Queue::default(),
            turn: Turn::default(),
            timers: Timers,
            reads: Reads,
            log: LogWrites,
            seed: Seeding,
            observer: None,
            events_since_snapshot: 0,
            revision,
        }
    }

    /// Same actor, publishing its durable history to `observer` — what a
    /// session agent needs and a workflow agent does not.
    pub fn with_observer(
        ctx: AgentRuntimeContext,
        params: AgentParams,
        observer: Arc<dyn AgentObserver>,
    ) -> Self {
        Self {
            observer: Some(observer),
            ..Self::new(ctx, params)
        }
    }

    /// The journal identity of an agent: kind `"agent"`, id = the agent's own
    /// [`AgentRuntimeContext::journal_id`]. Centralizes the kind so the
    /// workflow (e.g. sub session) and the actor agree.
    pub fn persistence_id_for(journal_id: uuid::Uuid) -> PersistenceId {
        PersistenceId::new("agent", journal_id.to_string())
    }

    /// Add the snapshot cadence to a component's effect: once enough events
    /// have accrued, the next persisting effect carries a snapshot along.
    ///
    /// Actor-level because it is storage policy, not a domain decision — a
    /// component still forces a snapshot itself where one is always right (a
    /// park, a seed), and the counter resets either way.
    fn with_snapshot_cadence(
        &mut self,
        effect: CommandEffect<AgentDomainEvent>,
    ) -> CommandEffect<AgentDomainEvent> {
        if effect.snapshots() {
            self.events_since_snapshot = 0;
            return effect;
        }
        if !effect.events().is_empty() && self.events_since_snapshot >= SNAPSHOT_EVERY_EVENTS {
            self.events_since_snapshot = 0;
            return effect.and_snapshot();
        }
        effect
    }
}

#[async_trait]
impl EventSourcedActor for AgentActor {
    type Command = AgentCommand;
    type Event = AgentDomainEvent;
    type State = AgentState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.runtime.journal_id)
    }

    fn initial_state() -> AgentState {
        AgentState::default()
    }

    /// Fold one event into state — the shared [`component::fold`], so live
    /// handling, replay and every component's own fold-forward agree.
    fn apply_event(state: AgentState, event: AgentDomainEvent) -> AgentState {
        fold(state, event)
    }

    /// Route each command to the component that owns it. The actor decides
    /// nothing here.
    async fn handle_command(
        &mut self,
        state: &AgentState,
        cmd: AgentCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        let mut cx = Cx {
            state,
            scratch: &mut self.scratch,
            runtime: &self.runtime,
            params: &self.params,
            actor: ctx,
        };
        let effect = match cmd {
            AgentCommand::Queue(c) => self.queue.handle(c, &mut cx).await,
            AgentCommand::Run(c) => self.turn.handle(c, &mut cx).await,
            AgentCommand::Timer(c) => self.timers.handle(c, &mut cx).await,
            AgentCommand::Read(c) => self.reads.handle(c, &mut cx).await,
            AgentCommand::Log(c) => self.log.handle(c, &mut cx).await,
            AgentCommand::Seed(c) => self.seed.handle(c, &mut cx).await,
            // Inlined rather than given a component: stopping is the actor's
            // whole answer, and there is no state, no event and no second
            // command to keep it company.
            AgentCommand::Core(CoreCommand::Shutdown) => CommandEffect::stop(),
        };
        self.with_snapshot_cadence(effect)
    }

    /// Publish what just became durable. By the time this runs the events are
    /// written and folded, so `state` already contains what they appended.
    async fn on_events_persisted(&mut self, events: &[AgentDomainEvent], state: &AgentState) {
        self.events_since_snapshot = self
            .events_since_snapshot
            .saturating_add(events.len() as u64);
        // An entry supersedes every chunk that preceded it — the finished
        // message says everything they were building towards — so the deltas
        // are dropped the moment one lands.
        if events.iter().any(coarse_appends_an_entry) {
            self.scratch.deltas.clear();
        }
        self.revision.send_modify(|r| *r += 1);
        let Some(observer) = &self.observer else {
            return;
        };
        for event in events {
            observer.publish(event, state);
        }
    }

    /// Repair whatever the crash left half-done, before the first live
    /// command. Each component repairs its own; the actor only asks.
    async fn on_recovery_complete(
        &mut self,
        state: &AgentState,
        ctx: &mut ActorContext<AgentCommand>,
    ) {
        // Announce where this incarnation starts. The channel outlives the
        // actor, so after an idle offload it still holds the position from
        // before — republishing costs nothing.
        self.revision.send_modify(|r| *r += 1);
        let mut cx = Cx {
            state,
            scratch: &mut self.scratch,
            runtime: &self.runtime,
            params: &self.params,
            actor: ctx,
        };
        self.timers.on_load(&mut cx).await;
        self.turn.on_load(&mut cx).await;
        // Last, so anything the turn set up (a self-continue's gate) is
        // already in the scratch the drain reads.
        self.queue.on_load(&mut cx).await;
    }
}
