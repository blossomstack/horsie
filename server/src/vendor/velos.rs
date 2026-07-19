//! Runtime vendor backed by a [velos](https://github.com/blossomstack)-scheduled
//! remote container.
//!
//! Unlike [`crate::vendor::LocalDaemonVendor`], which looks up an
//! already-connected daemon rather than spawning anything, this vendor asks
//! velos to run a container whose command is
//! `horsie-runtime --endpoint ws://<advertise_address>/api/runtime/connect …`.
//! The runtime dials back over that outbound connection (velos exposes no
//! inbound networking) to the server's HTTP port, where the `/api/runtime/connect`
//! route feeds it into the server-wide [`ConnectedRuntimeRegistry`] that this
//! vendor shares; the registry demultiplexes connections by `runtime_id`. The
//! vendor no longer owns a listener — only the [`RuntimeProvider`] differs from
//! the local path.
//!
//! velos containers are ephemeral (no volumes), so `stop` deletes the container
//! and `attach` schedules a fresh one under the same `runtime_id`; the durable
//! session state (the journal) lives server-side and recovers on attach.

use crate::velos::{ContainerApi, ContainerLaunchSpec};
use crate::vendor::{
    RuntimeSpec, RuntimeVendor, VendorCapabilities, VendorError, VendorRuntime, VendorRuntimeHandle,
};
use async_trait::async_trait;
use horsie_executor::{
    ConnectedRuntimeRegistry, HealthStatus, InMemExecutorTransport, RuntimeError, RuntimeHandle,
    RuntimeProvider,
};
use horsie_executor_client::ExecutorClient;
use horsie_models::executor::{RuntimeConfig, WorkspaceConfig};
use horsie_runtime_client::RuntimeClient;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Extra time granted on top of `connect_timeout` when a runtime has provision
/// steps to run (e.g. cloning) inside the container before it announces Ready.
const PROVISION_ALLOWANCE: Duration = Duration::from_secs(900);

/// Deployment-global knobs for the velos vendor. Built from config.
#[derive(Debug, Clone)]
pub struct VelosVendorSettings {
    /// OCI image bundling `horsie-runtime` (Linux, built without the sandbox
    /// feature — the container is the isolation boundary).
    pub image: String,
    /// Path to `horsie-runtime` inside the image.
    pub runtime_bin: String,
    /// In-container root under which each workspace is created (`<root>/<name>`).
    pub workspace_root: String,
    /// `host:port` the container dials back to — the server's externally
    /// reachable HTTP endpoint, published on the velos worker's container
    /// network. Both the reverse-dial WebSocket (`ws://<addr>/api/runtime/connect`)
    /// and the plugin-artifact fetch (`http://<addr>/...`) target this one
    /// address; there is no separate reverse-dial port anymore.
    pub advertise_address: String,
    pub cpu: u32,
    pub memory_bytes: u64,
    /// How long to wait for a scheduled container's runtime to dial back.
    pub connect_timeout: Duration,
}

/// One workspace, resolved to the directory it lives at inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceMount {
    name: String,
    container_path: String,
}

/// The velos object name for a runtime (its `metadata.name`).
fn container_name(runtime_id: &str) -> String {
    format!("horsie-{runtime_id}")
}

/// POSIX single-quote a value so it survives `sh -c` verbatim (embedded quotes
/// become `'\''`). Workspace names derive from user paths, so quote defensively.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Build the container command: create the workspace dirs, then `exec` the
/// runtime so it becomes PID 1 and its exit is the container's exit. Returns an
/// argv the image runs via `sh -c` (no `--sandbox-caps`/`--plugins-dir`/
/// `--hook-path`: the container is the sandbox and remote plugins are out of MVP
/// scope).
fn build_container_command(
    runtime_bin: &str,
    endpoint_ws: &str,
    runtime_id: &str,
    mounts: &[WorkspaceMount],
) -> Vec<String> {
    let mut exec_line = format!(
        "exec {} --endpoint {} --runtime-id {}",
        shell_quote(runtime_bin),
        shell_quote(endpoint_ws),
        shell_quote(runtime_id),
    );
    for m in mounts {
        exec_line.push_str(&format!(
            " --workspace {}",
            shell_quote(&format!("{}={}", m.name, m.container_path)),
        ));
    }
    let script = if mounts.is_empty() {
        exec_line
    } else {
        let dirs = mounts
            .iter()
            .map(|m| shell_quote(&m.container_path))
            .collect::<Vec<_>>()
            .join(" ");
        format!("mkdir -p {dirs} && {exec_line}")
    };
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

/// A [`RuntimeProvider`] that provisions a velos container instead of a local
/// child process. Constructed fresh per `create`/`attach` (like the local
/// path), sharing the vendor's `ContainerApi` and connection registry.
///
/// The `id` passed to [`RuntimeProvider::create`] is the **incarnation** id (a
/// per-attempt dial-back key on the shared registry), while `container_name` is
/// the **deterministic** velos object name for the session — so a stale
/// connection's cleanup can never unregister a fresh incarnation, yet the
/// container is still reclaimable by name after a server restart.
struct VelosRuntimeProvider {
    api: Arc<dyn ContainerApi>,
    connected: Arc<ConnectedRuntimeRegistry>,
    container_name: String,
    endpoint_ws: String,
    image: String,
    runtime_bin: String,
    cpu: u32,
    memory_bytes: u64,
    connect_timeout: Duration,
}

impl VelosRuntimeProvider {
    fn mounts(&self, config: &RuntimeConfig) -> Vec<WorkspaceMount> {
        config
            .workspaces
            .iter()
            .map(|w| WorkspaceMount {
                name: w.name.clone(),
                container_path: w.path.clone(),
            })
            .collect()
    }

    /// Wait for the runtime to dial back, polling velos so a container that dies
    /// before connecting fails fast instead of burning the whole timeout. `wait`
    /// is the deadline — longer when provision steps (clones) run before Ready.
    async fn await_ready(
        &self,
        runtime_id: &str,
        name: &str,
        ready_rx: tokio::sync::oneshot::Receiver<Result<(), String>>,
        wait: Duration,
    ) -> Result<(), RuntimeError> {
        tokio::pin!(ready_rx);
        let deadline = tokio::time::sleep(wait);
        tokio::pin!(deadline);
        let poll_period = Duration::from_millis(750);
        let mut poll =
            tokio::time::interval_at(tokio::time::Instant::now() + poll_period, poll_period);
        loop {
            tokio::select! {
                res = &mut ready_rx => {
                    return match res {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(message)) => Err(RuntimeError::Provider(message)),
                        Err(_) => Err(RuntimeError::Provider(
                            "runtime readiness channel dropped".to_string(),
                        )),
                    };
                }
                _ = &mut deadline => {
                    return Err(RuntimeError::Provider(format!(
                        "timed out waiting for runtime '{runtime_id}' to connect"
                    )));
                }
                _ = poll.tick() => {
                    if let Ok(Some(phase)) = self.api.container_phase(name).await
                        && phase.is_dead()
                    {
                        return Err(RuntimeError::Provider(format!(
                            "velos container '{name}' reached {phase:?} before connecting"
                        )));
                    }
                }
            }
        }
    }
}

#[async_trait]
impl RuntimeProvider for VelosRuntimeProvider {
    async fn create(
        &self,
        id: &str,
        config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
        // Register the readiness waiter BEFORE scheduling, mirroring the process
        // provider, so a fast dial-back can't race ahead of the waiter. `id` is
        // the incarnation id — the dial-back key the container announces.
        let ready_rx = self.connected.notify_when_ready(id).await;
        let command = build_container_command(
            &self.runtime_bin,
            &self.endpoint_ws,
            id,
            &self.mounts(config),
        );
        let mut env: BTreeMap<String, String> = config
            .env
            .iter()
            .map(|e| (e.name.clone(), e.value.clone()))
            .collect();
        if !config.provision.is_empty() {
            let json = serde_json::to_string(&config.provision)
                .map_err(|e| RuntimeError::Provider(format!("encode provision steps: {e}")))?;
            env.insert(horsie_models::ENV_PROVISION.to_string(), json);
        }
        let spec = ContainerLaunchSpec {
            image: self.image.clone(),
            command,
            env,
            cpu: self.cpu,
            memory_bytes: self.memory_bytes,
        };
        self.api
            .create_container(&self.container_name, &spec)
            .await
            .map_err(|e| RuntimeError::Provider(e.to_string()))?;

        // Provision steps (clones) may legitimately take minutes; the failure
        // path stays fast because ProvisionFailed resolves the waiter early.
        let wait = if config.provision.is_empty() {
            self.connect_timeout
        } else {
            self.connect_timeout + PROVISION_ALLOWANCE
        };
        if let Err(e) = self
            .await_ready(id, &self.container_name, ready_rx, wait)
            .await
        {
            // Reclaim the container we scheduled but never heard from.
            let _ = self.api.delete_container(&self.container_name).await;
            self.connected.remove(id).await;
            return Err(e);
        }

        Ok(Arc::new(VelosRuntimeHandle {
            api: self.api.clone(),
            connected: self.connected.clone(),
            name: self.container_name.clone(),
            runtime_id: id.to_string(),
        }))
    }
}

/// Lifecycle handle for one scheduled container. `stop` deletes it (velos has no
/// pause), and health follows the live dial-back connection.
struct VelosRuntimeHandle {
    api: Arc<dyn ContainerApi>,
    connected: Arc<ConnectedRuntimeRegistry>,
    name: String,
    runtime_id: String,
}

#[async_trait]
impl RuntimeHandle for VelosRuntimeHandle {
    async fn stop(&self) -> Result<(), RuntimeError> {
        let _ = self.api.delete_container(&self.name).await;
        self.connected.remove(&self.runtime_id).await;
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus, RuntimeError> {
        let connected = self
            .connected
            .runtime_transport(&self.runtime_id)
            .await
            .is_some();
        Ok(if connected {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy {
                reason: "runtime disconnected".to_string(),
            }
        })
    }
}

/// Settings that can change under a vendor's feet. Every field is live-editable
/// via [`VelosVendor::reconfigure`] — there is no listener to rebind, so unlike
/// the old design nothing is frozen, `advertise_address` included.
#[derive(Clone)]
pub struct VelosMutableSettings {
    pub api: Arc<dyn ContainerApi>,
    pub image: String,
    pub runtime_bin: String,
    pub workspace_root: String,
    pub cpu: u32,
    pub memory_bytes: u64,
    pub connect_timeout: Duration,
    /// `host:port` the container dials back to; both the reverse-dial WS URL
    /// and the plugin-artifact base are derived from it per `provision()`.
    pub advertise_address: String,
}

/// The reverse-dial WebSocket URL a scheduled runtime is told to connect to:
/// the server's HTTP endpoint plus the runtime-connect route.
fn endpoint_ws_for(advertise_address: &str) -> String {
    format!("ws://{advertise_address}/api/runtime/connect")
}

/// The velos runtime vendor. Shares the server-wide [`ConnectedRuntimeRegistry`]
/// (fed by the HTTP `/api/runtime/connect` route) rather than owning a listener.
pub struct VelosVendor {
    connected: Arc<ConnectedRuntimeRegistry>,
    settings: RwLock<VelosMutableSettings>,
}

impl VelosVendor {
    /// Build the vendor against the shared reverse-dial registry. No listener is
    /// bound — the HTTP server accepts runtime dial-backs and demultiplexes them
    /// into `connected` by `runtime_id`. `api` is how containers are
    /// scheduled/reclaimed.
    pub fn new(
        api: Arc<dyn ContainerApi>,
        settings: VelosVendorSettings,
        connected: Arc<ConnectedRuntimeRegistry>,
    ) -> Self {
        Self {
            connected,
            settings: RwLock::new(VelosMutableSettings {
                api,
                image: settings.image,
                runtime_bin: settings.runtime_bin,
                workspace_root: settings.workspace_root,
                cpu: settings.cpu,
                memory_bytes: settings.memory_bytes,
                connect_timeout: settings.connect_timeout,
                advertise_address: settings.advertise_address,
            }),
        }
    }

    /// Current mutable settings (a cheap clone under a read lock) — for
    /// inspection (tests, a future debug endpoint) and as the base a caller
    /// mutates before calling `reconfigure`.
    pub fn settings(&self) -> VelosMutableSettings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Swap in new mutable settings (e.g. after a live config edit). Never
    /// touches the listener — only the next `provision()` call sees the new
    /// values.
    pub fn reconfigure(&self, settings: VelosMutableSettings) {
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = settings;
    }

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
        // Deterministic container name (reclaimable by name after a restart) +
        // a unique per-attempt incarnation id (the dial-back key on the shared
        // registry, so a lingering old connection can't unregister this one).
        let container = container_name(runtime_id);
        let incarnation = format!("{runtime_id}-{}", uuid::Uuid::new_v4().simple());
        let (provider, workspace_root) = {
            let settings = self.settings.read().unwrap_or_else(|e| e.into_inner());
            let provider = Arc::new(VelosRuntimeProvider {
                api: settings.api.clone(),
                connected: self.connected.clone(),
                container_name: container,
                endpoint_ws: endpoint_ws_for(&settings.advertise_address),
                image: settings.image.clone(),
                runtime_bin: settings.runtime_bin.clone(),
                cpu: settings.cpu,
                memory_bytes: settings.memory_bytes,
                connect_timeout: settings.connect_timeout,
            });
            (provider, settings.workspace_root.clone())
        };
        let client = ExecutorClient::new(InMemExecutorTransport::new(
            provider,
            self.connected.clone(),
        ));
        let config = runtime_config_from(spec, &workspace_root);
        let result = if attach {
            client.attach_runtime(&incarnation, config).await
        } else {
            client.create_runtime(&incarnation, config).await
        };
        result.map_err(|e| wrap(e.to_string()))?;
        let transport = client
            .runtime_transport(&incarnation)
            .await
            .map_err(|e| wrap(e.to_string()))?;
        Ok(VendorRuntime {
            runtime_client: RuntimeClient::from_arc(transport),
            handle: Arc::new(VelosHandle {
                client,
                runtime_id: incarnation,
            }),
        })
    }
}

#[async_trait]
impl RuntimeVendor for VelosVendor {
    fn capabilities(&self) -> VendorCapabilities {
        // Schedules a fresh container and clones repos / installs bundles into
        // the managed workspace at provision time.
        VendorCapabilities {
            supports_provisioning: true,
        }
    }

    fn artifact_base_url(&self) -> Option<String> {
        // Same host:port the runtime dials back on, over HTTP — the worker
        // fetches plugin bundles from the server's one published endpoint.
        let addr = self
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .advertise_address
            .clone();
        Some(format!("http://{addr}"))
    }

    fn plugins_dir_for(&self, _runtime_id: &str) -> Option<String> {
        // A fixed in-container path; the container is ephemeral and isolated, so
        // one dir per runtime is unnecessary.
        Some("/horsie/plugins".to_string())
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
        // Callable with no live handle (e.g. after a server restart): reclaim the
        // container by its deterministic name. Any live incarnation's transport
        // was already removed by its handle's `stop` (the session halts before
        // delete), so there is nothing to unregister here.
        let api = self
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .api
            .clone();
        let _ = api.delete_container(&container_name(runtime_id)).await;
    }
}

/// The vendor-facing lifecycle handle: stop routes through the executor client
/// so the shared registry's bookkeeping stays consistent (which in turn deletes
/// the container via [`VelosRuntimeHandle::stop`]).
struct VelosHandle {
    client: ExecutorClient,
    runtime_id: String,
}

#[async_trait]
impl VendorRuntimeHandle for VelosHandle {
    async fn stop(&self) {
        let _ = self.client.stop_runtime(&self.runtime_id).await;
    }
}

/// Remote runtime config: `Managed` workspaces map to in-container paths under
/// the vendor's workspace root; host directories are impossible in a remote
/// container and rejected. Env and provision steps carry over; local-only
/// inputs (plugins/hooks) are dropped — the container is self-contained.
fn runtime_config_from(spec: &RuntimeSpec, workspace_root: &str) -> RuntimeConfig {
    let root = workspace_root.trim_end_matches('/');
    let workspaces = spec
        .workspaces
        .iter()
        .map(|w| WorkspaceConfig {
            name: w.name.clone(),
            path: format!("{root}/{}", w.name),
        })
        .collect();
    RuntimeConfig {
        workspaces,
        plugins_dir: None,
        hook_path: vec![],
        env: spec.env.clone(),
        provision: spec.provision.clone(),
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
    use crate::velos::{ContainerPhase, VelosError};
    use crate::vendor::WorkspaceSpec;
    use futures_util::{SinkExt, StreamExt};
    use horsie_executor::{RuntimeEndpoint, RuntimeListenerServer, serve_runtime_connections};
    use horsie_models::runtime::{
        BashInput, RuntimeInboundMessage, RuntimeOutboundMessage, RuntimeReady, ToolCall,
        ToolCallResponse, ToolOutput, ToolResult,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::task::JoinHandle;
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn command_creates_dirs_and_execs_runtime_without_sandbox_flags() {
        let mounts = vec![
            WorkspaceMount {
                name: "main".into(),
                container_path: "/workspace/main".into(),
            },
            WorkspaceMount {
                name: "docs".into(),
                container_path: "/workspace/docs".into(),
            },
        ];
        let cmd = build_container_command("horsie-runtime", "ws://10.0.0.1:7070", "rt-1", &mounts);
        assert_eq!(cmd[0], "/bin/sh");
        assert_eq!(cmd[1], "-c");
        let script = &cmd[2];
        assert!(script.starts_with("mkdir -p '/workspace/main' '/workspace/docs' &&"));
        assert!(script.contains("exec 'horsie-runtime'"));
        assert!(script.contains("--endpoint 'ws://10.0.0.1:7070'"));
        assert!(script.contains("--runtime-id 'rt-1'"));
        assert!(script.contains("--workspace 'main=/workspace/main'"));
        assert!(script.contains("--workspace 'docs=/workspace/docs'"));
        // The container is the sandbox — no nono, no shared plugins.
        assert!(!script.contains("--sandbox-caps"));
        assert!(!script.contains("--plugins-dir"));
        assert!(!script.contains("--hook-path"));
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    // --- Full reverse-dial over TCP with a fake velos + fake runtime ---------

    /// A `ContainerApi` double: instead of a micro-VM, `create_container` spawns
    /// an in-process WebSocket "runtime" that dials the vendor's listener and
    /// answers tool calls, exactly as a real container's `horsie-runtime` would.
    /// It learns *where* to dial and *what id* to announce by parsing the command
    /// the vendor built — just as the real container's `sh -c` would.
    struct FakeVelosApi {
        creates: Mutex<Vec<String>>,
        deletes: Mutex<Vec<String>>,
        /// The `--runtime-id` (incarnation id) each container was told to announce.
        incarnations: Mutex<Vec<String>>,
        /// The image each `create_container` call was asked to run — used to
        /// assert `reconfigure()` changes what the next `provision()` sees.
        images: Mutex<Vec<String>>,
        tasks: Mutex<HashMap<String, JoinHandle<()>>>,
    }

    impl FakeVelosApi {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                creates: Mutex::new(Vec::new()),
                deletes: Mutex::new(Vec::new()),
                incarnations: Mutex::new(Vec::new()),
                images: Mutex::new(Vec::new()),
                tasks: Mutex::new(HashMap::new()),
            })
        }
        fn creates(&self) -> Vec<String> {
            self.creates.lock().unwrap().clone()
        }
        fn deletes(&self) -> Vec<String> {
            self.deletes.lock().unwrap().clone()
        }
        fn incarnations(&self) -> Vec<String> {
            self.incarnations.lock().unwrap().clone()
        }
        fn images(&self) -> Vec<String> {
            self.images.lock().unwrap().clone()
        }
    }

    /// Pull the single-quoted value after `flag` out of the `sh -c` script.
    fn arg_after(script: &str, flag: &str) -> Option<String> {
        let marker = format!("{flag} '");
        let rest = script.split(&marker).nth(1)?;
        rest.split('\'').next().map(str::to_string)
    }

    async fn fake_runtime(endpoint: String, runtime_id: String) {
        let (ws, _) = match connect_async(&endpoint).await {
            Ok(x) => x,
            Err(_) => return,
        };
        let (mut sink, mut stream) = ws.split();
        let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
            runtime_id,
            workdir: "/workspace".to_string(),
        }))
        .unwrap();
        if sink.send(Message::Text(ready.into())).await.is_err() {
            return;
        }
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Text(text) = msg
                && let Ok(RuntimeInboundMessage::ToolCall(req)) =
                    serde_json::from_str::<RuntimeInboundMessage>(&text)
            {
                let resp = RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                    call_id: req.call_id,
                    result: ToolResult::Ok(ToolOutput {
                        stdout: "remote-ok".to_string(),
                        stderr: String::new(),
                        exit_code: 0,
                    }),
                });
                if let Ok(json) = serde_json::to_string(&resp) {
                    let _ = sink.send(Message::Text(json.into())).await;
                }
            }
        }
    }

    #[async_trait]
    impl ContainerApi for FakeVelosApi {
        async fn create_container(
            &self,
            name: &str,
            spec: &ContainerLaunchSpec,
        ) -> Result<(), VelosError> {
            self.creates.lock().unwrap().push(name.to_string());
            self.images.lock().unwrap().push(spec.image.clone());
            // Dial exactly where/how the vendor's command tells the container to.
            let script = spec.command.get(2).cloned().unwrap_or_default();
            let endpoint = arg_after(&script, "--endpoint").expect("endpoint in command");
            let runtime_id = arg_after(&script, "--runtime-id").expect("runtime-id in command");
            self.incarnations.lock().unwrap().push(runtime_id.clone());
            let handle = tokio::spawn(fake_runtime(endpoint, runtime_id));
            self.tasks.lock().unwrap().insert(name.to_string(), handle);
            Ok(())
        }

        async fn delete_container(&self, name: &str) -> Result<(), VelosError> {
            self.deletes.lock().unwrap().push(name.to_string());
            if let Some(t) = self.tasks.lock().unwrap().remove(name) {
                t.abort();
            }
            Ok(())
        }

        async fn container_phase(&self, name: &str) -> Result<Option<ContainerPhase>, VelosError> {
            Ok(if self.tasks.lock().unwrap().contains_key(name) {
                Some(ContainerPhase::Running)
            } else {
                None
            })
        }
    }

    fn test_settings(advertise_address: String) -> VelosVendorSettings {
        VelosVendorSettings {
            image: "test/image".into(),
            runtime_bin: "horsie-runtime".into(),
            workspace_root: "/workspace".into(),
            advertise_address,
            cpu: 1,
            memory_bytes: 536_870_912,
            connect_timeout: Duration::from_secs(5),
        }
    }

    fn test_spec() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![WorkspaceSpec {
                name: "main".into(),
            }],
            provision: vec![],
            env: vec![],
            capabilities_file: std::env::temp_dir().join("caps.json"),
            plugins_dir: None,
            hook_path: vec![],
        }
    }

    /// Bind a real reverse-dial listener onto a fresh shared registry and build
    /// the vendor against that registry, with `advertise_address` pointed at the
    /// listener. Mirrors how the HTTP `/api/runtime/connect` route feeds the
    /// server-wide registry in production. The serve loop runs until process
    /// exit (the cancel token is never fired), which is fine for a test.
    async fn bind_vendor(api: Arc<FakeVelosApi>) -> VelosVendor {
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let listener =
            RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
                .await
                .expect("bind listener");
        let addr = listener.tcp_addr().expect("tcp addr");
        serve_runtime_connections(listener, registry.clone(), CancellationToken::new());
        VelosVendor::new(api, test_settings(addr.to_string()), registry)
    }

    #[tokio::test]
    async fn reconfigure_swaps_settings_without_changing_endpoint() {
        let api = FakeVelosApi::new();
        let vendor = bind_vendor(api.clone()).await;
        let addr_before = vendor.settings().advertise_address.clone();

        vendor.create("rt-1", &test_spec()).await.expect("create");
        assert_eq!(api.images(), vec!["test/image".to_string()]);

        let mut new_settings = vendor.settings();
        new_settings.image = "test/image-v2".into();
        vendor.reconfigure(new_settings);

        vendor
            .create("rt-2", &test_spec())
            .await
            .expect("create after reconfigure");
        assert_eq!(
            api.images(),
            vec!["test/image".to_string(), "test/image-v2".to_string()]
        );
        assert_eq!(
            vendor.settings().advertise_address,
            addr_before,
            "reconfigure must not rebind the listener"
        );
    }

    #[tokio::test]
    async fn create_reverse_dials_and_tool_calls_round_trip() {
        let api = FakeVelosApi::new();
        let vendor = bind_vendor(api.clone()).await;

        let rt = vendor.create("rt-1", &test_spec()).await.expect("create");
        assert_eq!(api.creates(), vec!["horsie-rt-1"]);

        // A tool call round-trips over the real socket transport to the fake runtime.
        let out = rt
            .runtime_client
            .invoke(ToolCall::Bash(BashInput {
                command: "echo hi".into(),
                timeout_secs: None,
                workspace: None,
            }))
            .await
            .expect("tool call");
        assert_eq!(out.stdout, "remote-ok");

        // Stop deletes the container (velos has no pause).
        rt.handle.stop().await;
        assert_eq!(api.deletes(), vec!["horsie-rt-1"]);
    }

    #[tokio::test]
    async fn attach_schedules_a_fresh_container_then_delete_reclaims() {
        let api = FakeVelosApi::new();
        let vendor = bind_vendor(api.clone()).await;

        let rt = vendor.attach("rt-2", &test_spec()).await.expect("attach");
        assert_eq!(api.creates(), vec!["horsie-rt-2"]);
        rt.handle.stop().await;

        // Delete with no live handle still reclaims by deterministic name.
        vendor.delete("rt-2").await;
        // stop deleted once, delete deleted again (idempotent on the velos side).
        assert_eq!(api.deletes(), vec!["horsie-rt-2", "horsie-rt-2"]);
    }

    #[tokio::test]
    async fn stop_then_reattach_uses_a_distinct_incarnation_and_still_works() {
        // Guards the shared-registry race: create + attach for the same session
        // must dial back under *different* ids, so a lingering old connection's
        // close can never unregister the fresh incarnation's transport.
        let api = FakeVelosApi::new();
        let vendor = bind_vendor(api.clone()).await;

        let rt1 = vendor.create("rt-9", &test_spec()).await.expect("create");
        rt1.handle.stop().await;
        let rt2 = vendor.attach("rt-9", &test_spec()).await.expect("attach");

        let ids = api.incarnations();
        assert_eq!(ids.len(), 2, "one dial-back per provision");
        assert_ne!(ids[0], ids[1], "distinct incarnations for the same session");
        assert!(ids.iter().all(|id| id.starts_with("rt-9-")));
        // Both containers share the deterministic velos name.
        assert_eq!(api.creates(), vec!["horsie-rt-9", "horsie-rt-9"]);

        // The reattached incarnation's transport is live and round-trips.
        let out = rt2
            .runtime_client
            .invoke(ToolCall::Bash(BashInput {
                command: "x".into(),
                timeout_secs: None,
                workspace: None,
            }))
            .await
            .expect("tool call after reattach");
        assert_eq!(out.stdout, "remote-ok");
    }

    #[tokio::test]
    async fn create_fails_fast_when_container_dies_before_connecting() {
        // A ContainerApi that accepts the create but never dials back and reports
        // a dead phase — the provider must give up well before the timeout.
        struct DeadApi;
        #[async_trait]
        impl ContainerApi for DeadApi {
            async fn create_container(
                &self,
                _n: &str,
                _s: &ContainerLaunchSpec,
            ) -> Result<(), VelosError> {
                Ok(())
            }
            async fn delete_container(&self, _n: &str) -> Result<(), VelosError> {
                Ok(())
            }
            async fn container_phase(
                &self,
                _n: &str,
            ) -> Result<Option<ContainerPhase>, VelosError> {
                Ok(Some(ContainerPhase::Failed))
            }
        }
        let mut settings = test_settings("127.0.0.1:0".to_string());
        settings.connect_timeout = Duration::from_secs(30);
        // DeadApi never dials back, so no listener is needed — a bare shared
        // registry is enough to exercise the connect-timeout path.
        let vendor = VelosVendor::new(
            Arc::new(DeadApi),
            settings,
            Arc::new(ConnectedRuntimeRegistry::new()),
        );
        let result =
            tokio::time::timeout(Duration::from_secs(5), vendor.create("rt-3", &test_spec()))
                .await
                .expect("should not hang until the 30s connect timeout");
        match result {
            Err(VendorError::Provision(msg)) => {
                assert!(msg.contains("before connecting"), "{msg}")
            }
            Err(other) => panic!("expected Provision error, got {other:?}"),
            Ok(_) => panic!("dead container should fail create"),
        }
    }
}
