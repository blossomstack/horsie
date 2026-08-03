//! Durable journal backends for the session server's actor tree.
//!
//! The trait and the file/in-memory backends live in `horsie-actor`; this module
//! adds the SQL-backed one, which belongs here because it shares the settings
//! database and so its schema belongs to this crate's migration chain.

mod sqlite;

pub use sqlite::SqliteJournal;
