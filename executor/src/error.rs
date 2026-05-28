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

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("connection failed: {0}")]
    Connection(String),
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}
