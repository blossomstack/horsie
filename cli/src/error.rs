use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("io error: {0}")]
    Io(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("validation failed:\n{0}")]
    Validation(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("executor error: {0}")]
    Executor(String),
}
