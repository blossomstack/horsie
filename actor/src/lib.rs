//! Generic, domain-free event-sourced actor runtime.
//!
//! An [`EventSourcedActor`] rebuilds its state by replaying persisted events,
//! so a fresh actor with the same `persistence_id` recovers exactly where the
//! previous one left off — the foundation for crash recovery. The runtime owns
//! persistence and state transitions; actors only return [`CommandEffect`]s
//! describing what to persist.
//!
//! Neither agent nor workflow concepts appear here.

mod actor;
mod error;
mod journal;
mod runtime;

pub use actor::{CommandEffect, EventSourcedActor};
pub use error::{JournalError, TellError};
pub use journal::{InMemoryJournal, Journal, JournalResult};
pub use runtime::{ActorContext, ActorRef, spawn_root};
