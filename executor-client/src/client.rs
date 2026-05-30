use crate::transport::ExecutorTransport;
use models::executor::{
    CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand, ExecutorEvent, RuntimeConfig,
    RuntimeState,
};
use runtime_client::RuntimeTransport;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("disconnected")]
    Disconnected,
}

/// Lifecycle-only client to a connected executor (`create` / `destroy` /
/// `runtime_transport`). Tool calls go through the [`RuntimeTransport`] obtained
/// from [`ExecutorClient::runtime_transport`].
pub struct ExecutorClient {
    transport: Arc<dyn ExecutorTransport>,
}

impl ExecutorClient {
    pub fn new(transport: impl ExecutorTransport + 'static) -> Self {
        Self {
            transport: Arc::new(transport),
        }
    }

    pub fn from_arc(transport: Arc<dyn ExecutorTransport>) -> Self {
        Self { transport }
    }

    pub async fn create_runtime(&self, id: &str, config: RuntimeConfig) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(
                &req,
                ExecutorCommand::CreateRuntime(CreateRuntimeCmd {
                    runtime_id: id.to_string(),
                    config,
                }),
            )
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::RuntimeStateChanged(e)) if e.state == RuntimeState::Running => {
                    return Ok(());
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    return Err(ClientError::CommandFailed(e.message));
                }
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn destroy_runtime(&self, id: &str) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(
                &req,
                ExecutorCommand::DestroyRuntime(DestroyRuntimeCmd {
                    runtime_id: id.to_string(),
                }),
            )
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::RuntimeStateChanged(e)) if e.state == RuntimeState::Stopped => {
                    return Ok(());
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    return Err(ClientError::CommandFailed(e.message));
                }
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    /// Obtain the tool-call transport for `runtime_id` (direct in CLI mode, relay
    /// in server mode — the caller cannot tell).
    pub async fn runtime_transport(
        &self,
        runtime_id: &str,
    ) -> Result<Arc<dyn RuntimeTransport>, ClientError> {
        self.transport.runtime_transport(runtime_id).await
    }
}
