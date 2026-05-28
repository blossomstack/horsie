use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("executor not found: {0}")]
    ExecutorNotFound(String),
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("bind failed: {0}")]
    BindFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}
