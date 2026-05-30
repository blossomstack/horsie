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
pub enum CommandEffect<E> {
    /// Persist these events and fold them into the current state.
    Persist(Vec<E>),
    /// Persist and fold these events, then snapshot the resulting state and
    /// compact the now-redundant event log.
    PersistAndSnapshot(Vec<E>),
    /// Snapshot the current state and compact the event log; persist no new events.
    Snapshot,
    /// Do nothing.
    None,
    /// Stop the actor.
    Stop,
    /// Persist and fold these events, then stop the actor.
    PersistAndStop(Vec<E>),
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
