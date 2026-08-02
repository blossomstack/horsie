use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    error::RuntimeError,
    provider::{HealthStatus, RuntimeHandle},
    runtime_listener::RuntimeEndpoint,
};
use async_trait::async_trait;
use horsie_models::executor::{EnvVar, RuntimeConfig};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{process::Child, sync::Mutex};

/// Extra time granted on top of `connect_timeout` when a runtime has provision
/// steps to run (e.g. cloning) before it can announce Ready.
const PROVISION_ALLOWANCE: Duration = Duration::from_secs(900);

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

/// RuntimeProvider that spawns `horsie-runtime` as a child process. Transport- and
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

/// Apply the environment policy for a runtime child to `cmd`.
///
/// Sandboxed (`sandboxed == true`): `env_clear()` + the scrubbed ambient
/// allowlist (see [`crate::env_scrub`]), then the explicitly `injected` vars on
/// top. Injection wins over the ambient allowlist on conflict — e.g. an
/// injected `HOME` overrides the host `HOME`. This is the deliberate per-job
/// channel for job-scoped values like capability tokens and synthetic homes;
/// every ambient var NOT explicitly injected stays scrubbed, so orchestrator
/// secrets (e.g. `ANTHROPIC_API_KEY`) still never leak.
///
/// Unsandboxed: the child inherits the full ambient environment unchanged
/// (today's behavior), and the `injected` vars are still applied on top —
/// injection is explicit daemon intent, independent of sandboxing.
fn apply_child_env(cmd: &mut tokio::process::Command, sandboxed: bool, injected: &[EnvVar]) {
    if sandboxed {
        // Scrub the environment: the child must not inherit orchestrator secrets.
        cmd.env_clear();
        for (k, v) in crate::env_scrub::scrubbed_env() {
            cmd.env(k, v);
        }
    }
    for var in injected {
        cmd.env(&var.name, &var.value);
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
            .arg(id);
        for ws in &config.workspaces {
            cmd.arg("--workspace")
                .arg(format!("{}={}", ws.name, ws.path));
        }
        if let Some(dir) = &config.plugins_dir {
            cmd.arg("--plugins-dir").arg(dir);
        }
        for hp in &config.hook_path {
            cmd.arg("--hook-path").arg(hp);
        }

        if let Some(policy) = &self.sandbox {
            cmd.arg("--sandbox-caps").arg(&policy.capabilities_file);
        }
        let mut injected = config.env.clone();
        if !config.provision.is_empty() {
            let json = serde_json::to_string(&config.provision)
                .map_err(|e| RuntimeError::Provider(format!("encode provision steps: {e}")))?;
            injected.push(EnvVar {
                name: horsie_models::ENV_PROVISION.to_string(),
                value: json,
            });
        }
        apply_child_env(&mut cmd, self.sandbox.is_some(), &injected);

        cmd.kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| RuntimeError::Provider(e.to_string()))?;

        // Provision steps (clones) may legitimately take minutes; the failure
        // path stays fast because ProvisionFailed resolves the waiter early.
        let wait = if config.provision.is_empty() {
            self.connect_timeout
        } else {
            self.connect_timeout + PROVISION_ALLOWANCE
        };
        tokio::time::timeout(wait, ready_rx)
            .await
            .map_err(|_| RuntimeError::Provider("runtime connection timed out".to_string()))?
            .map_err(|_| RuntimeError::Provider("connection channel dropped".to_string()))?
            .map_err(RuntimeError::Provider)?;

        Ok(Arc::new(ProcessRuntimeHandle {
            child: Mutex::new(Some(child)),
            runtime_id: id.to_string(),
            connected_registry: Arc::clone(&self.connected_registry),
        }))
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

    fn which_bash() -> Option<std::path::PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join("bash"))
                .find(|p| p.exists())
        })
    }

    /// Spawn `bash -c <script>` with the given env policy and return its stdout.
    async fn child_stdout(script: &str, sandboxed: bool, injected: &[EnvVar]) -> String {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(script);
        apply_child_env(&mut cmd, sandboxed, injected);
        let out = cmd.output().await.unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[tokio::test]
    async fn sandboxed_child_receives_injected_vars() {
        if which_bash().is_none() {
            eprintln!("skipping: no bash on PATH");
            return;
        }
        let injected = vec![EnvVar {
            name: "HACKAMORE_TOKEN".to_string(),
            value: "tok-123".to_string(),
        }];
        let out = child_stdout("printf '%s' \"$HACKAMORE_TOKEN\"", true, &injected).await;
        assert_eq!(out, "tok-123");
    }

    /// Injection wins over the ambient allowlist on conflict: an injected HOME
    /// (a synthetic per-job home) overrides the host HOME the scrub re-added.
    #[tokio::test]
    async fn injected_home_overrides_host_home() {
        if which_bash().is_none() {
            eprintln!("skipping: no bash on PATH");
            return;
        }
        let host_home = std::env::var("HOME").unwrap_or_default();
        let injected = vec![EnvVar {
            name: "HOME".to_string(),
            value: "/synthetic/home".to_string(),
        }];
        let out = child_stdout("printf '%s' \"$HOME\"", true, &injected).await;
        assert_eq!(out, "/synthetic/home");
        assert_ne!(out, host_home, "test requires a distinct synthetic HOME");
    }

    /// Injection does not weaken the scrub: ambient secrets not explicitly
    /// injected are still wiped from the sandboxed child.
    #[tokio::test]
    async fn ambient_secret_still_scrubbed_when_other_vars_injected() {
        if which_bash().is_none() {
            eprintln!("skipping: no bash on PATH");
            return;
        }
        let injected = vec![EnvVar {
            name: "HACKAMORE_TOKEN".to_string(),
            value: "tok-123".to_string(),
        }];
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg("printf '%s' \"$ANTHROPIC_API_KEY\"");
        // Seed the secret before the scrub, simulating inheritance.
        cmd.env("ANTHROPIC_API_KEY", "leak-me");
        apply_child_env(&mut cmd, true, &injected);
        let out = cmd.output().await.unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "ANTHROPIC_API_KEY leaked despite not being injected"
        );
    }

    /// An empty injection list is exactly today's behavior: the sandboxed child
    /// sees only the scrubbed allowlist, nothing more.
    #[tokio::test]
    async fn empty_injection_matches_scrub_only_behavior() {
        if which_bash().is_none() {
            eprintln!("skipping: no bash on PATH");
            return;
        }
        let out = child_stdout("env", true, &[]).await;
        let expected: std::collections::BTreeMap<String, String> =
            crate::env_scrub::scrubbed_env().into_iter().collect();
        for line in out.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue; // multi-line value continuation
            };
            // bash sets a few of its own vars (PWD, SHLVL, _, ...); everything
            // it *inherited* must come from the allowlist with ambient values.
            if let Some(exp) = expected.get(k) {
                assert_eq!(v, exp, "allowlisted {k} differs from ambient value");
            } else {
                assert!(
                    !crate::env_scrub::SANDBOX_ENV_ALLOWLIST.contains(&k)
                        && ["PWD", "OLDPWD", "SHLVL", "_"].contains(&k),
                    "unexpected inherited var in scrubbed child: {k}"
                );
            }
        }
    }

    /// Injection applies in the unsandboxed path too (explicit daemon intent),
    /// while the ambient environment is still inherited unchanged.
    #[tokio::test]
    async fn unsandboxed_child_receives_injected_vars_and_inherits_ambient() {
        if which_bash().is_none() {
            eprintln!("skipping: no bash on PATH");
            return;
        }
        let injected = vec![EnvVar {
            name: "HACKAMORE_TOKEN".to_string(),
            value: "tok-456".to_string(),
        }];
        let out = child_stdout(
            "printf '%s:%s' \"$HACKAMORE_TOKEN\" \"$PATH\"",
            false,
            &injected,
        )
        .await;
        let (tok, path) = out.split_once(':').unwrap();
        assert_eq!(tok, "tok-456");
        assert_eq!(path, std::env::var("PATH").unwrap_or_default());
    }
}
