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
#[cfg(feature = "file-journal")]
mod file_journal;
mod journal;
mod persistence_id;
mod runtime;

pub use actor::{CommandEffect, EventSourcedActor};
pub use error::{JournalError, TellError};
#[cfg(feature = "file-journal")]
pub use file_journal::FileJournal;
pub use journal::{InMemoryJournal, Journal, JournalResult};
pub use persistence_id::PersistenceId;
pub use runtime::{ActorContext, ActorRef, spawn_root};
