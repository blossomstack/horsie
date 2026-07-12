//! Runtime vendor backed by a nono-sandboxed `horsie-runtime` child process.
//!
//! Each create/attach builds a fresh executor assembly (runtime listener +
//! connected registry + process provider + in-mem transport), exactly like the
//! daemon's `ProcessJobRuntime`. Stop kills the child but preserves all on-disk
//! state (workspace + capability file), so attach can respawn against it.

use crate::vendor::{
    RuntimeSpec, RuntimeVendor, VendorError, VendorRuntime, VendorRuntimeHandle, WorkspaceSource,
};
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
    /// Root under which `Managed` workspaces are allocated
    /// (`<workspace_root>/<runtime_id>/<name>`), deterministic so attach
    /// re-finds them and delete can reclaim them.
    workspace_root: PathBuf,
    /// Server HTTP base a co-located runtime fetches plugin artifacts from
    /// (loopback). `None` disables plugin provisioning for this vendor.
    public_http_base: Option<String>,
}

impl LocalProcessVendor {
    pub fn new(
        runtime_bin: PathBuf,
        workspace_root: PathBuf,
        public_http_base: Option<String>,
    ) -> Self {
        Self {
            runtime_bin,
            workspace_root,
            public_http_base,
        }
    }

    /// Root for materialized bundles, `<workspace_root>/.plugins`. Granted
    /// read/write in the session capability spec so the sandboxed runtime can
    /// unpack and scan there.
    fn plugins_root(&self) -> PathBuf {
        self.workspace_root.join(".plugins")
    }

    /// Resolve workspace sources to concrete host paths, creating managed dirs.
    fn resolve_workspaces(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<Vec<WorkspaceConfig>, String> {
        spec.workspaces
            .iter()
            .map(|w| {
                let path = match &w.source {
                    WorkspaceSource::HostDir(p) => p.clone(),
                    WorkspaceSource::Managed => {
                        let dir = self.workspace_root.join(runtime_id).join(&w.name);
                        std::fs::create_dir_all(&dir)
                            .map_err(|e| format!("allocate workspace '{}': {e}", w.name))?;
                        dir
                    }
                };
                Ok(WorkspaceConfig {
                    name: w.name.clone(),
                    path: path.to_string_lossy().into_owned(),
                })
            })
            .collect()
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
        let workspaces = self.resolve_workspaces(runtime_id, spec).map_err(wrap)?;
        let config = runtime_config_from(spec, workspaces);
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

    fn artifact_base_url(&self) -> Option<String> {
        self.public_http_base.clone()
    }

    fn plugins_dir_for(&self, runtime_id: &str) -> Option<String> {
        Some(
            self.plugins_root()
                .join(runtime_id)
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn plugins_cache_dir(&self) -> Option<String> {
        Some(
            self.plugins_root()
                .join(".cache")
                .to_string_lossy()
                .into_owned(),
        )
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
        // Bring-your-own host dirs are never touched; managed allocations for
        // this runtime are reclaimed best-effort.
        let dir = self.workspace_root.join(runtime_id);
        if dir.is_dir()
            && let Err(e) = std::fs::remove_dir_all(&dir)
        {
            tracing::warn!(runtime_id, error = %e, "failed to reclaim managed workspace");
        }
    }
}

fn runtime_config_from(spec: &RuntimeSpec, workspaces: Vec<WorkspaceConfig>) -> RuntimeConfig {
    RuntimeConfig {
        workspaces,
        plugins_dir: spec
            .plugins_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        hook_path: spec
            .hook_path
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        env: spec.env.clone(),
        provision: spec.provision.clone(),
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::vendor::{WorkspaceSource, WorkspaceSpec};

    #[test]
    fn managed_workspace_allocates_deterministically_and_host_dir_passes_through() {
        let root = tempfile::tempdir().unwrap();
        let vendor =
            LocalProcessVendor::new("horsie-runtime".into(), root.path().to_path_buf(), None);
        let spec = RuntimeSpec {
            workspaces: vec![
                WorkspaceSpec {
                    name: "main".into(),
                    source: WorkspaceSource::Managed,
                },
                WorkspaceSpec {
                    name: "byo".into(),
                    source: WorkspaceSource::HostDir("/home/u/api".into()),
                },
            ],
            provision: vec![],
            env: vec![],
            capabilities_file: root.path().join("caps.json"),
            plugins_dir: None,
            hook_path: vec![],
        };
        let ws = vendor.resolve_workspaces("rt-1", &spec).unwrap();
        let managed = root.path().join("rt-1").join("main");
        assert_eq!(ws[0].path, managed.to_string_lossy());
        assert!(managed.is_dir(), "managed dir is created");
        assert_eq!(ws[1].path, "/home/u/api");
        // Same runtime_id resolves to the same dir (attach re-finds it).
        let again = vendor.resolve_workspaces("rt-1", &spec).unwrap();
        assert_eq!(again[0].path, ws[0].path);
    }
}
