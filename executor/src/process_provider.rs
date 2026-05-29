use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    error::RuntimeError,
    provider::{HealthStatus, RuntimeHandle},
};
use async_trait::async_trait;
use models::executor::RuntimeConfig;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{process::Child, sync::Mutex};

pub struct ProcessRuntimeHandle {
    child: Mutex<Option<Child>>,
    runtime_id: String,
    connected_registry: Arc<ConnectedRuntimeRegistry>,
}

#[async_trait]
impl RuntimeHandle for ProcessRuntimeHandle {
    async fn stop(&self) -> Result<(), RuntimeError> {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
        self.connected_registry.remove(&self.runtime_id).await;
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus, RuntimeError> {
        let connected = self
            .connected_registry
            .get_sink(&self.runtime_id)
            .await
            .is_some();
        if connected {
            Ok(HealthStatus::Healthy)
        } else {
            Ok(HealthStatus::Unhealthy {
                reason: "runtime disconnected".to_string(),
            })
        }
    }
}

/// RuntimeProvider that spawns `october-runtime` as a child process.
pub struct ProcessRuntimeProvider {
    binary_path: PathBuf,
    listener_addr: SocketAddr,
    connected_registry: Arc<ConnectedRuntimeRegistry>,
    connect_timeout: Duration,
}

impl ProcessRuntimeProvider {
    pub fn new(
        binary_path: PathBuf,
        listener_addr: SocketAddr,
        connected_registry: Arc<ConnectedRuntimeRegistry>,
    ) -> Self {
        Self {
            binary_path,
            listener_addr,
            connected_registry,
            connect_timeout: Duration::from_secs(30),
        }
    }

    pub fn with_connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }
}

#[async_trait]
impl crate::provider::RuntimeProvider for ProcessRuntimeProvider {
    async fn create(
        &self,
        id: &str,
        config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
        // Register a watcher BEFORE spawning to avoid a race.
        let ready_rx = self.connected_registry.notify_when_ready(id).await;

        let child = tokio::process::Command::new(&self.binary_path)
            .arg("--executor-url")
            .arg(format!("ws://{}", self.listener_addr))
            .arg("--runtime-id")
            .arg(id)
            .arg("--working-dir")
            .arg(&config.working_dir)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RuntimeError::Provider(e.to_string()))?;

        tokio::time::timeout(self.connect_timeout, ready_rx)
            .await
            .map_err(|_| RuntimeError::Provider("runtime connection timed out".to_string()))?
            .map_err(|_| RuntimeError::Provider("connection channel dropped".to_string()))?;

        Ok(Arc::new(ProcessRuntimeHandle {
            child: Mutex::new(Some(child)),
            runtime_id: id.to_string(),
            connected_registry: Arc::clone(&self.connected_registry),
        }))
    }
}
