use crate::connected_registry::ConnectedRuntimeRegistry;
use crate::executor::create_core;
use crate::provider::RuntimeProvider;
use crate::registry::RuntimeRegistry;
use async_trait::async_trait;
use horsie_executor_client::{ClientError, ExecutorTransport};
use horsie_models::executor::{
    CommandFailedEvent, ExecutorCommand, ExecutorEvent, RuntimeState, RuntimeStateChangedEvent,
};
use horsie_runtime_client::RuntimeTransport;
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

    /// Shared stop path for Destroy / Stop / Delete: halt the child, drop the
    /// registry entry, report `Stopped`.
    async fn stop_core(&self, runtime_id: &str) -> ExecutorEvent {
        match self.registry.begin_stop(runtime_id).await {
            Ok(handle) => {
                if let Some(h) = handle {
                    let _ = h.stop().await;
                }
                let _ = self.registry.complete_stop(runtime_id).await;
                ExecutorEvent::RuntimeStateChanged(RuntimeStateChangedEvent {
                    runtime_id: runtime_id.to_string(),
                    state: RuntimeState::Stopped,
                })
            }
            Err(e) => ExecutorEvent::CommandFailed(CommandFailedEvent {
                message: e.to_string(),
            }),
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
                let ev = self.stop_core(&c.runtime_id).await;
                let _ = tx.send(ev).await;
            }
            // Stop-preserve: the in-process side is identical to destroy (kill the
            // child); preservation is the caller's on-disk state. Distinct wire
            // signal so richer vendors can diverge without a protocol change.
            ExecutorCommand::StopRuntime(c) => {
                let ev = self.stop_core(&c.runtime_id).await;
                let _ = tx.send(ev).await;
            }
            // Attach: a local process cannot resume in place — revive by
            // provisioning a fresh child against the preserved config.
            ExecutorCommand::AttachRuntime(c) => {
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
            // Delete: the owning session is gone; this executor tears the process
            // down (the user's workspace is never touched).
            ExecutorCommand::DeleteRuntime(c) => {
                let ev = self.stop_core(&c.runtime_id).await;
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::error::RuntimeError;
    use crate::provider::{HealthStatus, RuntimeHandle};

    struct InstantHandle;

    #[async_trait]
    impl RuntimeHandle for InstantHandle {
        async fn stop(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn health_check(&self) -> Result<HealthStatus, RuntimeError> {
            Ok(HealthStatus::Healthy)
        }
    }

    struct InstantProvider;

    #[async_trait]
    impl RuntimeProvider for InstantProvider {
        async fn create(
            &self,
            _id: &str,
            _c: &horsie_models::executor::RuntimeConfig,
        ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
            Ok(Arc::new(InstantHandle))
        }
    }

    fn cfg() -> horsie_models::executor::RuntimeConfig {
        horsie_models::executor::RuntimeConfig {
            workspaces: vec![],
            plugins_dir: None,
            hook_path: vec![],
            env: vec![],
        }
    }

    #[tokio::test]
    async fn stop_attach_delete_signals_round_trip() {
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        let provider: Arc<dyn RuntimeProvider> = Arc::new(InstantProvider);
        let t = InMemExecutorTransport::new(provider, connected);
        let client = horsie_executor_client::ExecutorClient::new(t);
        client.create_runtime("r1", cfg()).await.unwrap();
        client.stop_runtime("r1").await.unwrap();
        // After stop-preserve, attach revives under the same id.
        client.attach_runtime("r1", cfg()).await.unwrap();
        client.delete_runtime("r1").await.unwrap();
    }

    #[tokio::test]
    async fn stop_unknown_runtime_fails() {
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        let provider: Arc<dyn RuntimeProvider> = Arc::new(InstantProvider);
        let t = InMemExecutorTransport::new(provider, connected);
        let client = horsie_executor_client::ExecutorClient::new(t);
        assert!(client.stop_runtime("nope").await.is_err());
    }
}
