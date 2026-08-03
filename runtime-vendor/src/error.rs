use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime already exists: {0}")]
    AlreadyExists(String),
    #[error("runtime not found: {0}")]
    NotFound(String),
    #[error("invalid state transition from {from}: cannot {action}")]
    InvalidTransition { from: String, action: String },
    #[error("provider error: {0}")]
    Provider(String),
}

/// Why a credential could not be produced for a dial, split by what the
/// reconnect loop should do about it.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// The issuer could not be reached. Indistinguishable from a failed dial
    /// as far as the agent is concerned, and retried the same way.
    #[error("{0}")]
    Transient(String),
    /// The credential is definitively dead — revoked, expired past recovery,
    /// or refused by the issuer. No amount of retrying will fix it, so the
    /// agent stops and says so rather than looping on a 401 forever.
    #[error("{0}")]
    Dead(String),
}

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("connection failed: {0}")]
    Connection(String),
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("bind failed: {0}")]
    BindFailed(String),
    #[error("spawn failed: {0}")]
    SpawnFailed(String),
}
