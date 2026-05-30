use thiserror::Error;

/// Errors surfaced by [`Journal`](crate::Journal) operations.
#[derive(Debug, Error)]
pub enum JournalError {
    /// The underlying storage backend failed.
    #[error("journal backend error: {0}")]
    Backend(String),

    /// An event or snapshot could not be (de)serialized.
    #[error("journal serialization error: {0}")]
    Serialization(String),
}

/// Error returned when delivering a command to an actor's mailbox fails.
#[derive(Debug, Error)]
pub enum TellError {
    /// The target actor has stopped and its mailbox is closed.
    #[error("actor mailbox closed")]
    MailboxClosed,
}
