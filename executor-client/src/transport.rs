use crate::client::ClientError;
use async_trait::async_trait;
use models::executor::{ExecutorCommand, ExecutorEvent};
use runtime_client::RuntimeTransport;
use std::sync::Arc;
use tokio::sync::mpsc;

/// How an [`ExecutorClient`](crate::ExecutorClient) reaches an executor.
///
/// Deep-module boundary: the caller drives lifecycle through `send` and obtains a
/// tool-call transport through `runtime_transport`, never learning whether the
/// executor is in-process (CLI → direct unix socket) or remote (server → relay).
#[async_trait]
pub trait ExecutorTransport: Send + Sync {
    /// Send a lifecycle command; returns a channel yielding events for this request.
    async fn send(
        &self,
        request_id: &str,
        cmd: ExecutorCommand,
    ) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError>;

    /// Obtain the tool-call transport for `runtime_id`.
    async fn runtime_transport(
        &self,
        runtime_id: &str,
    ) -> Result<Arc<dyn RuntimeTransport>, ClientError>;
}
