//! The shape every component of a session shares.
//!
//! A component owns a slice of [`SessionState`], the commands that change it,
//! and the events that record the change. It never mutates: `handle` decides
//! and returns events, the actor persists them, and `apply` folds them. That is
//! the discipline `EventSourcedActor` imposes on the actor as a whole, applied
//! one level down — and it is what makes a crash mid-command safe, because
//! nothing happened unless it was journaled.
//!
//! A component may **read** any part of the state: `Turns` reads the subagent
//! forest to learn which finished results ride its next turn. It may **write**
//! only its own slice, through its own events. Reading across is what keeps
//! components from having to talk to each other; writing across is what this
//! trait exists to prevent.
//!
//! Crucially, a component never learns which *kind* of session it is in. There
//! is no `if workflow` below this trait. `Turns` goes quiet when `state.run` is
//! set and `WorkflowRun` goes quiet when it is not, and each reaches that by
//! reading state rather than by being told.
//!
//! # Why every method is an associated function
//!
//! [`EventSourcedActor::apply_event`] takes no `self`: the runtime folds with no
//! instance in scope, which is precisely what guarantees replay is reproducible
//! from the journal alone. A component's fold inherits that constraint, and
//! once the fold cannot see an instance there is nothing left for the others to
//! usefully hold — configuration lives in `state` and in the session's spec,
//! where it already was. So components are unit structs, dispatch is a `match`
//! rather than a registry, and there is no `Box<dyn Component>` anywhere.
//!
//! `handle` is deliberately *not* on this trait. It is async, it differs in
//! command type per component, and it needs `&mut SessionActor`; putting it here
//! would buy nothing but a generic bound. The trait covers the pure half, which
//! is the half that benefits from being uniform.

use super::{AgentAction, SessionCommand, SessionDomainEvent, SessionState};
use crate::sessions::spec::SessionSpec;
use uuid::Uuid;

/// What a component needs besides the folded state to decide what to start.
///
/// Both halves are deterministic: the id is fixed, and the spec is a snapshot
/// taken at creation. So `actions` stays a pure function of things that do not
/// change under it — a workflow's own definition lives here, not in the journal,
/// because the run was started from it rather than deriving it.
pub(super) struct ActionCx<'a> {
    pub id: Uuid,
    pub spec: &'a SessionSpec,
}

pub(super) trait Component {
    /// Fold one of this component's events into its slice of state.
    ///
    /// Must be pure — no I/O, no clock, no randomness. Anything it reads comes
    /// from the event or from state.
    fn apply(_state: &mut SessionState, _event: &SessionDomainEvent) {}

    /// What this component wants started, given the state as it now is.
    ///
    /// The other half of `handle`, and the reason a session gets anything done:
    /// most work here is not triggered by a command. A run advances because the
    /// previous step concluded; a finished subagent's result is delivered
    /// because it finished. Nobody asked for either — an event landed, the
    /// state changed, and something became startable.
    ///
    /// Called at every turn boundary, on every component, and the results are
    /// concatenated. A component returns only work **it** owns, which is why
    /// the boundary is a concatenation rather than a negotiation.
    ///
    /// Pure, like `apply`. The actor performs what this returns; deciding and
    /// performing stay apart so the decision is testable against a hand-built
    /// state with no actor, no runtime and no journal.
    fn actions(_cx: &ActionCx<'_>, _state: &SessionState) -> Vec<AgentAction> {
        Vec::new()
    }

    /// One command this component wants to send itself once recovery finishes,
    /// to repair what a dead process left behind.
    ///
    /// A self-send rather than direct work: recovery must not persist, and this
    /// runs before the first live command, so anything needing to journal has
    /// to arrive as an ordinary command.
    fn on_load(_cx: &ActionCx<'_>, _state: &SessionState) -> Option<SessionCommand> {
        None
    }

    /// Whether this component has work in flight, so the session must not
    /// unload. Asked of every component and OR-ed together — which is what lets
    /// a component added later make itself heard, where today's single
    /// hand-written condition could not.
    fn busy(_state: &SessionState) -> bool {
        false
    }
}
