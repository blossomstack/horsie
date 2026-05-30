use crate::connected_registry::ConnectedRuntimeRegistry;
use crate::executor::create_core;
use crate::provider::RuntimeProvider;
use crate::registry::RuntimeRegistry;
use async_trait::async_trait;
use executor_client::{ClientError, ExecutorTransport};
use models::executor::{
    CommandFailedEvent, ExecutorCommand, ExecutorEvent, RuntimeState, RuntimeStateChangedEvent,
};
use runtime_client::RuntimeTransport;
use std::sync::Arc;
use tokio::sync::mpsc;

/// In-process executor transport for CLI mode. Drives runtime lifecycle directly
/// against an owned `RuntimeRegistry` + provider (no WS hop), and returns the live
/// direct `RuntimeTransport` from the shared `ConnectedRuntimeRegistry`. The
/// distributed relay bridge is never exercised here.
pub struct InMemExecutorTransport {
    registry: Arc<RuntimeRegistry>,
    provider: Arc<dyn RuntimeProvider>,
    connected: Arc<ConnectedRuntimeRegistry>,
}

impl InMemExecutorTransport {
    /// `provider` must implement [`RuntimeProvider`] (satisfied by
    /// `ProcessRuntimeProvider`); `connected` is the registry the runtime listener
    /// registers transports into.
    pub fn new(
        provider: Arc<dyn RuntimeProvider>,
        connected: Arc<ConnectedRuntimeRegistry>,
    ) -> Self {
        Self {
            registry: Arc::new(RuntimeRegistry::new()),
            provider,
            connected,
        }
    }
}

#[async_trait]
impl ExecutorTransport for InMemExecutorTransport {
    async fn send(
        &self,
        _request_id: &str,
        cmd: ExecutorCommand,
    ) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError> {
        let (tx, rx) = mpsc::channel(8);
        match cmd {
            ExecutorCommand::CreateRuntime(c) => {
                let ev = match create_core(&self.registry, &self.provider, &c.runtime_id, c.config)
                    .await
                {
                    Ok(()) => ExecutorEvent::RuntimeStateChanged(RuntimeStateChangedEvent {
                        runtime_id: c.runtime_id,
                        state: RuntimeState::Running,
                    }),
                    Err(e) => ExecutorEvent::CommandFailed(CommandFailedEvent {
                        message: e.to_string(),
                    }),
                };
                let _ = tx.send(ev).await;
            }
            ExecutorCommand::DestroyRuntime(c) => {
                let ev = match self.registry.begin_stop(&c.runtime_id).await {
                    Ok(handle) => {
                        if let Some(h) = handle {
                            let _ = h.stop().await;
                        }
                        let _ = self.registry.complete_stop(&c.runtime_id).await;
                        ExecutorEvent::RuntimeStateChanged(RuntimeStateChangedEvent {
                            runtime_id: c.runtime_id,
                            state: RuntimeState::Stopped,
                        })
                    }
                    Err(e) => ExecutorEvent::CommandFailed(CommandFailedEvent {
                        message: e.to_string(),
                    }),
                };
                let _ = tx.send(ev).await;
            }
            ExecutorCommand::RestartRuntime(_)
            | ExecutorCommand::QueryRuntimes(_)
            | ExecutorCommand::ToolCall(_)
            | ExecutorCommand::CancelToolCall(_) => {
                let _ = tx
                    .send(ExecutorEvent::CommandFailed(CommandFailedEvent {
                        message: "command not supported by in-process executor".to_string(),
                    }))
                    .await;
            }
        }
        Ok(rx)
    }

    async fn runtime_transport(
        &self,
        runtime_id: &str,
    ) -> Result<Arc<dyn RuntimeTransport>, ClientError> {
        self.connected
            .runtime_transport(runtime_id)
            .await
            .ok_or_else(|| {
                ClientError::CommandFailed(format!("runtime '{runtime_id}' not connected"))
            })
    }
}
