//! Runtime vendor backed by a nono-sandboxed `horsie-runtime` child process.
//!
//! Each create/attach builds a fresh executor assembly (runtime listener +
//! connected registry + process provider + in-mem transport), exactly like the
//! daemon's `ProcessJobRuntime`. Stop kills the child but preserves all on-disk
//! state (workspace + capability file), so attach can respawn against it.

use crate::vendor::{RuntimeSpec, RuntimeVendor, VendorError, VendorRuntime, VendorRuntimeHandle};
use async_trait::async_trait;
use horsie_executor::{
    ConnectedRuntimeRegistry, InMemExecutorTransport, ProcessRuntimeProvider, RuntimeEndpoint,
    RuntimeListenerServer, SandboxPolicy, serve_runtime_connections,
};
use horsie_executor_client::ExecutorClient;
use horsie_models::executor::{RuntimeConfig, WorkspaceConfig};
use horsie_runtime_client::RuntimeClient;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct LocalProcessVendor {
    runtime_bin: PathBuf,
}

impl LocalProcessVendor {
    pub fn new(runtime_bin: PathBuf) -> Self {
        Self { runtime_bin }
    }

    /// Build one executor assembly and provision the child, signalling either
    /// `CreateRuntime` or `AttachRuntime` on the wire.
    async fn provision(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        attach: bool,
    ) -> Result<VendorRuntime, VendorError> {
        let wrap = |e: String| {
            if attach {
                VendorError::Attach(e)
            } else {
                VendorError::Provision(e)
            }
        };
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        let sock = socket_path().map_err(wrap)?;
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Unix(sock.clone()))
            .await
            .map_err(|e| wrap(e.to_string()))?;
        let cancel = CancellationToken::new();
        serve_runtime_connections(listener, connected.clone(), cancel.clone());

        let provider = ProcessRuntimeProvider::new(
            self.runtime_bin.clone(),
            RuntimeEndpoint::Unix(sock),
            connected.clone(),
        )
        .with_sandbox(SandboxPolicy {
            capabilities_file: spec.capabilities_file.clone(),
        });
        let client =
            ExecutorClient::new(InMemExecutorTransport::new(Arc::new(provider), connected));
        let config = runtime_config_from(spec);
        let result = if attach {
            client.attach_runtime(runtime_id, config).await
        } else {
            client.create_runtime(runtime_id, config).await
        };
        result.map_err(|e| wrap(e.to_string()))?;
        let transport = client
            .runtime_transport(runtime_id)
            .await
            .map_err(|e| wrap(e.to_string()))?;
        Ok(VendorRuntime {
            runtime_client: RuntimeClient::from_arc(transport),
            handle: Arc::new(LocalHandle {
                client,
                cancel,
                runtime_id: runtime_id.to_string(),
            }),
        })
    }
}

#[async_trait]
impl RuntimeVendor for LocalProcessVendor {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        self.provision(runtime_id, spec, false).await
    }

    async fn attach(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        self.provision(runtime_id, spec, true).await
    }

    async fn delete(&self, runtime_id: &str) {
        // Nothing beyond what stop released: the user's workspace is never
        // touched, and per-session server state dirs are owned by the session
        // layer. This vendor keeps preserved state until the OS cleans tmp.
        tracing::debug!(runtime_id, "local vendor delete: nothing to reclaim");
    }
}

fn runtime_config_from(spec: &RuntimeSpec) -> RuntimeConfig {
    RuntimeConfig {
        workspaces: spec
            .workspaces
            .iter()
            .map(|w| WorkspaceConfig {
                name: w.name.clone(),
                path: w.path.to_string_lossy().into_owned(),
            })
            .collect(),
        plugins_dir: spec
            .plugins_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        hook_path: spec
            .hook_path
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        env: vec![],
    }
}

/// Ephemeral unix socket for one executor assembly, kept short (sockaddr_un caps
/// the path at ~108 bytes) and unique per call so concurrent sessions never
/// collide. Mirrors the daemon's `ProcessJobRuntime`.
fn socket_path() -> Result<PathBuf, String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let path = std::env::temp_dir()
        .join(format!("horsie-{}", &token[..12]))
        .join("rt.sock");
    let max = if cfg!(target_os = "macos") { 103 } else { 107 };
    if path.as_os_str().len() > max {
        return Err(format!(
            "unix socket path too long ({} bytes, max {max}): {}",
            path.as_os_str().len(),
            path.display()
        ));
    }
    Ok(path)
}

struct LocalHandle {
    client: ExecutorClient,
    cancel: CancellationToken,
    runtime_id: String,
}

#[async_trait]
impl VendorRuntimeHandle for LocalHandle {
    async fn stop(&self) {
        // Explicit stop-preserve wire signal, then release the listener.
        let _ = self.client.stop_runtime(&self.runtime_id).await;
        self.cancel.cancel();
    }
}
