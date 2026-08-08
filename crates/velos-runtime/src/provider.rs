//! The velos-specific half of this agent: a [`RuntimeProvider`] that schedules
//! a container per runtime instead of spawning a local process.
//!
//! velos exposes no inbound networking, so the container's `horsie-runtime`
//! dials *back* over an outbound WebSocket to this agent's advertise address.
//! Containers are ephemeral (no volumes), so `stop` deletes the container and a
//! later attach schedules a fresh one against the same request; the durable
//! session state lives server-side and recovers on attach.

use crate::velos::{ContainerApi, ContainerLaunchSpec};
use async_trait::async_trait;
use horsie_models::executor::RuntimeConfig;
use horsie_runtime_vendor::{
    ConnectedRuntimeRegistry, HealthStatus, RuntimeError, RuntimeHandle, RuntimeProvider,
    WorkspaceResolver,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Extra time granted on top of `connect_timeout` when a runtime has provision
/// steps (e.g. cloning) to run inside the container before it announces Ready.
const PROVISION_ALLOWANCE: Duration = Duration::from_secs(900);

/// Workspaces velos allocates: any name is accepted and becomes
/// `<root>/<name>` inside the container. The opposite of the local agent, which
/// serves only the directories the user named.
pub struct ManagedWorkspaces {
    root: String,
}

impl ManagedWorkspaces {
    #[must_use]
    pub fn new(root: String) -> Self {
        Self {
            root: root.trim_end_matches('/').to_string(),
        }
    }
}

impl WorkspaceResolver for ManagedWorkspaces {
    fn resolve(&self, name: &str) -> Option<PathBuf> {
        // Reject path traversal: a workspace name comes from session config and
        // must not escape the root it is allocated under.
        if name.is_empty() || name.contains('/') || name.contains("..") {
            return None;
        }
        Some(PathBuf::from(format!("{}/{name}", self.root)))
    }
}

/// The velos object name for a runtime (its `metadata.name`). Deterministic, so
/// a container is reclaimable by name after an agent restart.
fn container_name(runtime_id: &str) -> String {
    format!("horsie-{runtime_id}")
}

/// POSIX single-quote a value so it survives `sh -c` verbatim (embedded quotes
/// become `'\''`). Workspace paths derive from user input, so quote defensively.
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
/// runtime so it becomes PID 1 and its exit is the container's exit. No
/// `--sandbox-caps`: the container is the isolation boundary.
pub fn build_container_command(
    runtime_bin: &str,
    endpoint_ws: &str,
    runtime_id: &str,
    workspaces: &[(String, String)],
) -> Vec<String> {
    let mut exec_line = format!(
        "exec {} --endpoint {} --runtime-id {}",
        shell_quote(runtime_bin),
        shell_quote(endpoint_ws),
        shell_quote(runtime_id),
    );
    for (name, path) in workspaces {
        exec_line.push_str(&format!(
            " --workspace {}",
            shell_quote(&format!("{name}={path}")),
        ));
    }
    let script = if workspaces.is_empty() {
        exec_line
    } else {
        let dirs = workspaces
            .iter()
            .map(|(_, path)| shell_quote(path))
            .collect::<Vec<_>>()
            .join(" ");
        format!("mkdir -p {dirs} && {exec_line}")
    };
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

pub struct VelosProviderSettings {
    /// OCI image bundling `horsie-runtime` (Linux, built without the sandbox
    /// feature — the container is the isolation boundary).
    pub image: String,
    /// Path to `horsie-runtime` inside the image.
    pub runtime_bin: String,
    /// `ws://host:port/` this agent is reachable at *from velos's container
    /// network*. Containers dial it to reach the agent's runtime listener.
    pub advertise_ws: String,
    pub cpu: u32,
    pub memory_bytes: u64,
    pub connect_timeout: Duration,
}

pub struct VelosContainerProvider {
    api: Arc<dyn ContainerApi>,
    connected: Arc<ConnectedRuntimeRegistry>,
    settings: VelosProviderSettings,
}

impl VelosContainerProvider {
    #[must_use]
    pub fn new(
        api: Arc<dyn ContainerApi>,
        connected: Arc<ConnectedRuntimeRegistry>,
        settings: VelosProviderSettings,
    ) -> Self {
        Self {
            api,
            connected,
            settings,
        }
    }

    /// Wait for the runtime to dial back, polling velos so a container that dies
    /// before connecting fails fast instead of burning the whole timeout.
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
impl RuntimeProvider for VelosContainerProvider {
    async fn create(
        &self,
        id: &str,
        config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
        let name = container_name(id);
        // Register the readiness waiter BEFORE scheduling, so a fast dial-back
        // cannot race ahead of the waiter.
        let ready_rx = self.connected.notify_when_ready(id).await;

        let workspaces: Vec<(String, String)> = config
            .workspaces
            .iter()
            .map(|w| (w.name.clone(), w.path.clone()))
            .collect();
        let command = build_container_command(
            &self.settings.runtime_bin,
            &self.settings.advertise_ws,
            id,
            &workspaces,
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

        // Reclaim any container left over under this name (an agent restart, or
        // a re-create after a crash) before scheduling a fresh one.
        let _ = self.api.delete_container(&name).await;
        self.api
            .create_container(
                &name,
                &ContainerLaunchSpec {
                    image: self.settings.image.clone(),
                    command,
                    env,
                    cpu: self.settings.cpu,
                    memory_bytes: self.settings.memory_bytes,
                },
            )
            .await
            .map_err(|e| RuntimeError::Provider(e.to_string()))?;

        // Provision steps (clones) may legitimately take minutes; the failure
        // path stays fast because ProvisionFailed resolves the waiter early.
        let wait = if config.provision.is_empty() {
            self.settings.connect_timeout
        } else {
            self.settings.connect_timeout + PROVISION_ALLOWANCE
        };
        if let Err(e) = self.await_ready(id, &name, ready_rx, wait).await {
            let _ = self.api.delete_container(&name).await;
            self.connected.remove(id).await;
            return Err(e);
        }

        Ok(Arc::new(VelosRuntimeHandle {
            api: self.api.clone(),
            connected: self.connected.clone(),
            name,
            runtime_id: id.to_string(),
        }))
    }
}

/// Lifecycle handle for one scheduled container. `stop` deletes it — velos has
/// no pause — and health follows the live dial-back connection.
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn command_creates_dirs_and_execs_the_runtime_without_sandbox_flags() {
        let cmd = build_container_command(
            "horsie-runtime",
            "ws://10.0.0.1:7070",
            "rt-1",
            &[
                ("main".to_string(), "/workspace/main".to_string()),
                ("docs".to_string(), "/workspace/docs".to_string()),
            ],
        );
        assert_eq!(cmd[0], "/bin/sh");
        assert_eq!(cmd[1], "-c");
        let script = &cmd[2];
        assert!(script.starts_with("mkdir -p '/workspace/main' '/workspace/docs' &&"));
        assert!(script.contains("exec 'horsie-runtime'"));
        assert!(script.contains("--endpoint 'ws://10.0.0.1:7070'"));
        assert!(script.contains("--runtime-id 'rt-1'"));
        assert!(script.contains("--workspace 'main=/workspace/main'"));
        // The container is the sandbox.
        assert!(!script.contains("--sandbox-caps"));
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    #[test]
    fn managed_workspaces_allocate_under_the_root() {
        let ws = ManagedWorkspaces::new("/workspace/".to_string());
        assert_eq!(
            ws.resolve("main"),
            Some(PathBuf::from("/workspace/main")),
            "any name is allocatable; velos owns the filesystem"
        );
    }

    #[test]
    fn managed_workspaces_refuse_names_that_escape_the_root() {
        let ws = ManagedWorkspaces::new("/workspace".to_string());
        assert_eq!(ws.resolve(".."), None);
        assert_eq!(ws.resolve("a/b"), None);
        assert_eq!(ws.resolve(""), None);
    }
}
