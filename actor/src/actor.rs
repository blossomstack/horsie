use crate::error::JournalError;
use crate::persistence_id::PersistenceId;
use crate::runtime::ActorContext;
use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// What an actor wants the runtime to do after handling a command.
///
/// The runtime owns persistence and state transitions; an actor expresses its
/// intent by returning one of these rather than mutating state directly. State
/// only ever changes by folding persisted events through
/// [`EventSourcedActor::apply_event`], guaranteeing that live operation and
/// crash recovery follow the exact same path.
///
/// An effect is one persist step (possibly empty) followed by an ordered set of
/// post-persist actions — snapshot, ack, stop — that the runtime runs only after
/// the durable write. Modelled as composable fields rather than one variant per
/// combination (à la Akka's `Effect.persist(e).thenRun(..).thenReply(..).thenStop()`),
/// so e.g. "persist + snapshot + ack" needs no new variant. Build with the
/// constructors ([`none`](Self::none), [`persist`](Self::persist),
/// [`snapshot`](Self::snapshot), [`stop`](Self::stop)) and chain post-actions with
/// [`and_snapshot`](Self::and_snapshot), [`and_ack`](Self::and_ack),
/// [`and_stop`](Self::and_stop).
pub struct CommandEffect<E> {
    /// Events to persist and fold (empty = persist nothing).
    pub(crate) events: Vec<E>,
    /// After the durable write, snapshot the state and compact the event log.
    pub(crate) snapshot: bool,
    /// After the durable write, report its outcome on this channel — `Ok(())` once
    /// the events are durably written, or `Err(JournalError)` if the write failed —
    /// so an [`ActorRef::ask`] caller gets true post-persist backpressure and can
    /// abort on failure. Events are neither folded nor counted on a failed write.
    pub(crate) ack: Option<tokio::sync::oneshot::Sender<Result<(), JournalError>>>,
    /// After the durable write, stop the actor.
    pub(crate) stop: bool,
}

impl<E> CommandEffect<E> {
    /// Do nothing.
    pub fn none() -> Self {
        Self {
            events: Vec::new(),
            snapshot: false,
            ack: None,
            stop: false,
        }
    }

    /// Persist and fold `events` into the current state.
    pub fn persist(events: Vec<E>) -> Self {
        Self {
            events,
            snapshot: false,
            ack: None,
            stop: false,
        }
    }

    /// Snapshot the current state and compact the event log; persist no new events.
    pub fn snapshot() -> Self {
        Self::none().and_snapshot()
    }

    /// Stop the actor; persist no new events.
    pub fn stop() -> Self {
        Self::none().and_stop()
    }

    /// Persist and fold `events`, then stop the actor.
    pub fn persist_and_stop(events: Vec<E>) -> Self {
        Self::persist(events).and_stop()
    }

    /// After the persist, snapshot the resulting state and compact the event log.
    #[must_use]
    pub fn and_snapshot(mut self) -> Self {
        self.snapshot = true;
        self
    }

    /// After the persist, report the durable-write outcome on `ack` (post-persist
    /// backpressure for an [`ActorRef::ask`] caller).
    #[must_use]
    pub fn and_ack(mut self, ack: tokio::sync::oneshot::Sender<Result<(), JournalError>>) -> Self {
        self.ack = Some(ack);
        self
    }

    /// After the persist, stop the actor.
    #[must_use]
    pub fn and_stop(mut self) -> Self {
        self.stop = true;
        self
    }
}

/// An actor whose state is rebuilt by replaying persisted events.
///
/// Implementors define three associated types — the commands they accept, the
/// events they persist, and the state those events fold into — plus the pure
/// `apply_event` fold and an async `handle_command` that decides what to persist.
#[async_trait]
pub trait EventSourcedActor: Send + Sized + 'static {
    /// Messages the actor accepts. Not persisted.
    type Command: Send + 'static;
    /// Facts the actor persists. Replayed on recovery.
    type Event: Send + Serialize + DeserializeOwned + 'static;
    /// State rebuilt by folding events. Snapshotable. `Sync` because the runtime
    /// lends `&State` to `handle_command` across await points.
    type State: Send + Sync + Serialize + DeserializeOwned + 'static;

    /// Identity under which this actor's events and snapshots are stored — its kind
    /// (actor type) plus a per-instance id. See [`PersistenceId`].
    fn persistence_id(&self) -> PersistenceId;

    /// The state of an actor with no persisted history.
    fn initial_state() -> Self::State;

    /// Pure fold of one event into state — used identically during replay and
    /// live operation. Must not perform side effects.
    fn apply_event(state: Self::State, event: Self::Event) -> Self::State;

    /// Handle one command. May spawn children, message other actors, or run work;
    /// returns what the runtime should persist.
    async fn handle_command(
        &mut self,
        state: &Self::State,
        cmd: Self::Command,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<Self::Event>;

    /// Hook invoked once after recovery completes, before the first live command.
    async fn on_recovery_complete(&mut self, _state: &Self::State, _ctx: &mut ActorContext<Self>) {}
}
