//! The shape every module of an agent shares.
//!
//! Two methods, not the session's four. `apply` folds one module's events into
//! state; `on_load` repairs what a dead process left behind. The session's
//! `actions` and `busy` are absent because neither has a second implementor
//! here: only the queue decides to start a turn, and an agent's busy-ness is
//! `running || preparing` — fields on the actor, not facts in the journal. A
//! trait method with one implementor is ceremony; both are cheap to add the day
//! a second one appears.
//!
//! The discipline is the one
//! [`EventSourcedActor`](horsie_actor::EventSourcedActor) imposes on the actor
//! as a whole, applied one level down: `handle` decides and returns events,
//! the actor persists them, and `apply` folds them. Nothing happened unless it
//! was journaled, so a crash mid-command is safe.
//!
//! A module may **read** any part of the state and **write** only through its
//! own events. One thing it cannot own is a field: `AgentState.log` is a shared
//! append-only spine that eleven of the twenty-one event variants push to. So
//! an agent's modules divide by *stage of a turn* — queue, prepare, run,
//! conclude — plus the side registers (timers, the task list, usage) and the
//! read paths, rather than by slice of state the way a session's do. A module
//! owns the entries it authors, not a field.
//!
//! `handle` is deliberately not on this trait, for the same reason it is not on
//! the session's: it is async, it differs in command type per module, and it
//! needs `&mut AgentActor`. Putting it here would buy nothing but a generic
//! bound. The trait covers the pure-and-recovery half.

use super::{AgentActor, AgentCommand, AgentDomainEvent, AgentState};
use async_trait::async_trait;
use horsie_actor::ActorContext;

#[async_trait]
pub(super) trait Component {
    /// Fold one of this module's events into state.
    ///
    /// Must be pure — no I/O, no clock, no randomness. Anything it reads comes
    /// from the event or from state. Takes the event by value because an
    /// agent's events carry whole sessions and messages, and a fold that
    /// had to clone them would pay for the whole transcript on every replay.
    fn apply(_state: &mut AgentState, _event: AgentDomainEvent) {}

    /// Repair whatever a dead process left this module holding, once recovery
    /// has finished and before the first live command is handled.
    ///
    /// Runs on the actor rather than returning a command because not every
    /// repair is a self-send: re-arming a timer spawns a sleep, and reporting
    /// an interrupted turn goes to the *parent*, which must happen from here so
    /// it is ordered ahead of anything queued while the actor was loading.
    /// Anything that needs to journal still has to arrive as an ordinary
    /// command, because recovery must not persist.
    async fn on_load(
        _actor: &mut AgentActor,
        _state: &AgentState,
        _ctx: &ActorContext<AgentCommand>,
    ) {
    }
}
