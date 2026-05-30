use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    error::RuntimeError,
    provider::{HealthStatus, RuntimeHandle},
    runtime_listener::RuntimeEndpoint,
};
use async_trait::async_trait;
use models::executor::RuntimeConfig;
use std::{path::PathBuf, sync::Arc, time::Duration};
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
            .runtime_transport(&self.runtime_id)
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

/// Sandbox policy passed to a spawned runtime child. Its presence means
/// "sandbox-on"; absence means "no nono" (today's server / test behavior). The
/// `capabilities_file` fully defines the allowed capabilities — the caller resolves
/// custom-or-default and writes a concrete file before spawning.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Capability file passed to the runtime as `--sandbox-caps`.
    pub capabilities_file: PathBuf,
}

/// RuntimeProvider that spawns `october-runtime` as a child process. Transport- and
/// sandbox-agnostic: it spawns the binary with whatever endpoint + sandbox policy it
/// was constructed with.
pub struct ProcessRuntimeProvider {
    binary_path: PathBuf,
    endpoint: RuntimeEndpoint,
    connected_registry: Arc<ConnectedRuntimeRegistry>,
    connect_timeout: Duration,
    sandbox: Option<SandboxPolicy>,
}

impl ProcessRuntimeProvider {
    pub fn new(
        binary_path: PathBuf,
        endpoint: RuntimeEndpoint,
        connected_registry: Arc<ConnectedRuntimeRegistry>,
    ) -> Self {
        Self {
            binary_path,
            endpoint,
            connected_registry,
            connect_timeout: Duration::from_secs(30),
            sandbox: None,
        }
    }

    pub fn with_connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }

    /// Spawn the child confined by nono (env-scrubbed + `--sandbox-caps`).
    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox = Some(policy);
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
        // Register a watcher BEFORE spawning to avoid losing the ready signal.
        let ready_rx = self.connected_registry.notify_when_ready(id).await;

        let endpoint_arg = match &self.endpoint {
            RuntimeEndpoint::Tcp(addr) => format!("ws://{addr}"),
            RuntimeEndpoint::Unix(path) => format!("unix:{}", path.display()),
        };

        let mut cmd = tokio::process::Command::new(&self.binary_path);
        cmd.arg("--endpoint")
            .arg(&endpoint_arg)
            .arg("--runtime-id")
            .arg(id)
            .arg("--working-dir")
            .arg(&config.working_dir);

        if let Some(policy) = &self.sandbox {
            cmd.arg("--sandbox-caps").arg(&policy.capabilities_file);
            // Scrub the environment: the child must not inherit orchestrator secrets.
            cmd.env_clear();
            for (k, v) in crate::env_scrub::scrubbed_env() {
                cmd.env(k, v);
            }
        }

        cmd.kill_on_drop(true);
        let child = cmd
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
