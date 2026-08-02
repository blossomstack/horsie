//! Runtime lifecycle, owned server-side and kept off every actor mailbox.
//!
//! The session and agent actors do not know how a runtime comes to exist, how
//! it is resumed, or how it goes away. They ask for a client and get one, or
//! they get one of two errors that mean genuinely different things
//! ([`RuntimeError::Unavailable`] is retryable; [`RuntimeError::Gone`] is
//! terminal). Everything else — resolving the vendor, assembling the spec,
//! minting short-lived credentials — happens here.
//!
//! **Created once.** [`RuntimeManager::create`] has exactly one caller, at
//! session creation, and that is the whole of the "provision only once"
//! guarantee: it is structural, not bookkept. [`RuntimeManager::get`] can
//! never provision, so no later code path can silently rebuild a workspace the
//! user believes still exists.

use crate::runtime_vendor::{
    RuntimeSpec, RuntimeVendorLink, VendorError, VendorRuntime, WorkspaceSpec,
};
use crate::sessions::spec::{SessionSpec, SharedVendors};
use horsie_runtime_client::RuntimeClient;
use std::path::PathBuf;
use std::sync::Arc;

/// What can go wrong acquiring a runtime, split by what the session should do
/// about it.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The vendor itself is not reachable — not registered, or its socket is
    /// dead. Always retryable: a laptop agent offline for ten minutes must
    /// never cost a session permanently.
    #[error("runtime vendor unavailable: {0}")]
    Unavailable(String),
    /// A live vendor says this session's runtime cannot be produced. Terminal
    /// for the session.
    #[error("runtime is gone: {0}")]
    Gone(String),
    /// A create could not provision. The session can try again.
    #[error("runtime provisioning failed: {0}")]
    Provision(String),
}

/// What the manager needs from the server to assemble a runtime spec.
#[derive(Clone)]
pub struct RuntimeDeps {
    pub vendors: SharedVendors,
    /// Per-session server state (capability files) under `<state_dir>/sessions/<id>/`.
    pub state_dir: PathBuf,
    pub github_tokens: Option<Arc<dyn crate::github::GithubTokenMinter>>,
    pub plugins: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
}

pub struct RuntimeManager {
    deps: RuntimeDeps,
}

impl RuntimeManager {
    #[must_use]
    pub fn new(deps: RuntimeDeps) -> Self {
        Self { deps }
    }

    fn vendor(&self, vendor: &str) -> Result<Arc<RuntimeVendorLink>, RuntimeError> {
        let links =
            self.deps.vendors.read().map_err(|_| {
                RuntimeError::Unavailable("vendor registry lock poisoned".to_string())
            })?;
        let link = links.get(vendor).cloned().ok_or_else(|| {
            RuntimeError::Unavailable(format!("unknown runtime vendor '{vendor}'"))
        })?;
        if !link.is_connected() {
            return Err(RuntimeError::Unavailable(format!(
                "vendor '{vendor}' is not connected"
            )));
        }
        Ok(link)
    }

    /// Write the capability file and assemble the vendor-facing spec.
    ///
    /// Re-assembled on every create rather than cached: the GitHub token and
    /// the plugin token are short-lived, and a stale one is worse than none.
    async fn runtime_spec(
        &self,
        session: &str,
        spec: &SessionSpec,
        vendor: &Arc<RuntimeVendorLink>,
    ) -> Result<RuntimeSpec, RuntimeError> {
        let dir = self.deps.state_dir.join("sessions").join(session);
        std::fs::create_dir_all(&dir).map_err(|e| RuntimeError::Provision(e.to_string()))?;
        let caps_path = dir.join("capabilities.json");
        std::fs::write(
            &caps_path,
            serde_json::to_vec_pretty(&spec.capabilities)
                .map_err(|e| RuntimeError::Provision(e.to_string()))?,
        )
        .map_err(|e| RuntimeError::Provision(e.to_string()))?;

        let mut rt_spec = RuntimeSpec {
            workspaces: spec
                .workspaces
                .iter()
                .map(|w| WorkspaceSpec {
                    name: w.name.clone(),
                })
                .collect(),
            provision: spec
                .provision
                .iter()
                .map(|s| horsie_models::executor::ProvisionStep {
                    name: s.name.clone(),
                    uses: s.uses.clone(),
                    with: s
                        .with
                        .iter()
                        .map(|(k, v)| horsie_models::executor::StepParam {
                            key: k.clone(),
                            value: v.clone(),
                        })
                        .collect(),
                })
                .collect(),
            env: vec![],
            capabilities_file: caps_path,
        };

        // A fresh, scoped token authorizing this session's `git_checkout`
        // provision steps. Never persisted.
        if let Some(minter) = &self.deps.github_tokens {
            let urls: Vec<String> = rt_spec
                .provision
                .iter()
                .filter(|s| s.uses == "git_checkout")
                .filter_map(|s| {
                    s.with
                        .iter()
                        .find(|p| p.key == "url")
                        .map(|p| p.value.clone())
                })
                .collect();
            if !urls.is_empty() {
                let token = minter
                    .mint_for(&urls)
                    .await
                    .map_err(RuntimeError::Provision)?;
                if let Some(token) = token {
                    rt_spec.env.push(horsie_models::executor::EnvVar {
                        name: horsie_models::ENV_GITHUB_TOKEN.to_string(),
                        value: token,
                    });
                }
            }
        }

        // Resolve the session's selected bundles to fetch refs plus a scoped
        // token; the runtime reads both from its environment at startup. Only
        // vendors that provision a workspace participate.
        if let Some(prov) = self.deps.plugins.as_ref()
            && vendor.capabilities().supports_provisioning
        {
            let mut names = spec.plugins.clone();
            if names.is_empty() {
                names = prov.default_names().await;
            }
            if !names.is_empty() {
                let refs = prov
                    .resolve(&names)
                    .await
                    .map_err(RuntimeError::Provision)?;
                let hashes: Vec<String> = refs.iter().map(|r| r.hash.clone()).collect();
                let token = prov.mint_token(session, &hashes);
                let manifest = serde_json::to_string(&refs)
                    .map_err(|e| RuntimeError::Provision(e.to_string()))?;
                rt_spec.env.push(horsie_models::executor::EnvVar {
                    name: horsie_models::ENV_PLUGIN_MANIFEST.to_string(),
                    value: manifest,
                });
                rt_spec.env.push(horsie_models::executor::EnvVar {
                    name: horsie_models::ENV_PLUGINS_TOKEN.to_string(),
                    value: token,
                });
            }
        }

        Ok(rt_spec)
    }

    /// Provision this session's runtime. One caller, once per session.
    pub async fn create(
        &self,
        session: &str,
        vendor: &str,
        spec: &SessionSpec,
    ) -> Result<(), RuntimeError> {
        let link = self.vendor(vendor)?;
        let rt_spec = self.runtime_spec(session, spec, &link).await?;
        link.create(session, &rt_spec)
            .await
            .map(|_| ())
            .map_err(|e: VendorError| match e {
                VendorError::Gone(m) => RuntimeError::Gone(m),
                VendorError::Unavailable(m) => RuntimeError::Unavailable(m),
                VendorError::Provision(m) => RuntimeError::Provision(m),
            })
    }

    /// Hand back a client for this session's runtime, resuming it if the
    /// vendor hibernated it. Never provisions.
    pub async fn get(&self, session: &str, vendor: &str) -> Result<RuntimeClient, RuntimeError> {
        let link = self.vendor(vendor)?;
        let runtime: VendorRuntime = link.get(session).await.map_err(|e| match e {
            VendorError::Gone(m) => RuntimeError::Gone(m),
            VendorError::Unavailable(m) => RuntimeError::Unavailable(m),
            VendorError::Provision(m) => RuntimeError::Provision(m),
        })?;
        Ok(runtime.runtime_client)
    }

    /// Advisory: the session is going cold. Best effort — a vendor that is not
    /// there simply misses the hint, and nothing about the session changes.
    pub async fn hibernate(&self, session: &str, vendor: &str) {
        if let Ok(link) = self.vendor(vendor) {
            link.hibernate(session).await;
        }
    }

    /// The session was deleted; the vendor decides the runtime's fate.
    pub async fn delete(&self, session: &str, vendor: &str) {
        if let Ok(link) = self.vendor(vendor) {
            link.delete(session).await;
        }
    }

    /// A cheap handle bound to one session, for whoever needs to execute.
    #[must_use]
    pub fn provider(self: &Arc<Self>, session: String, vendor: String) -> RuntimeClientProvider {
        RuntimeClientProvider {
            manager: self.clone(),
            session,
            vendor,
        }
    }
}

/// The agent's view of the runtime: one method, no lifecycle.
#[derive(Clone)]
pub struct RuntimeClientProvider {
    manager: Arc<RuntimeManager>,
    session: String,
    vendor: String,
}

impl RuntimeClientProvider {
    /// A working client for this session's runtime, resumed if need be.
    pub async fn get(&self) -> Result<RuntimeClient, RuntimeError> {
        self.manager.get(&self.session, &self.vendor).await
    }
}

/// A `RuntimeManager` over the same vendor map and state dir the deps carry.
/// Test-only: production builds it once in `main`.
#[cfg(test)]
pub(crate) fn test_runtime_manager(
    vendors: &crate::sessions::spec::SharedVendors,
    state_dir: &std::path::Path,
) -> std::sync::Arc<crate::runtime_manager::RuntimeManager> {
    std::sync::Arc::new(crate::runtime_manager::RuntimeManager::new(
        crate::runtime_manager::RuntimeDeps {
            vendors: vendors.clone(),
            state_dir: state_dir.to_path_buf(),
            github_tokens: None,
            plugins: None,
        },
    ))
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
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use crate::sessions::spec::{AgentSettings, SessionSpec, WorkspaceDef};
    use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
    use std::collections::HashMap;
    use std::sync::RwLock;

    fn session_spec(vendor: &str) -> SessionSpec {
        SessionSpec {
            name: None,
            agent: AgentSettings {
                model: "mock".into(),
                allowed_tools: None,
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
                mcp_servers: vec![],
                memory_spaces: vec![],
                thinking_effort: None,
            },
            workspaces: vec![WorkspaceDef {
                name: "main".into(),
            }],
            provision: vec![],
            capabilities: CapabilitySpec {
                network: NetworkPolicy::Block(BlockNetwork {}),
                grants: vec![],
                unsafe_seatbelt_rules: None,
            },
            vendor: vendor.into(),
            plugins: vec![],
        }
    }

    fn manager(tmp: &tempfile::TempDir, vendors: SharedVendors) -> Arc<RuntimeManager> {
        Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors,
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            plugins: None,
        }))
    }

    fn published(agent: &FakeRuntimeVendor, name: &str) -> SharedVendors {
        let mut map = HashMap::new();
        map.insert(name.to_string(), agent.link());
        Arc::new(RwLock::new(map))
    }

    #[tokio::test]
    async fn unavailable_when_the_vendor_name_is_not_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let m = manager(&tmp, Arc::new(RwLock::new(HashMap::new())));
        let Err(err) = m.get("s1", "nope").await else {
            panic!("an unregistered vendor must not yield a client")
        };
        assert!(
            matches!(err, RuntimeError::Unavailable(_)),
            "a missing vendor is retryable, never terminal: {err:?}"
        );
    }

    #[tokio::test]
    async fn unavailable_when_the_link_is_disconnected() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(&tmp, published(&agent, "v"));
        agent.disconnect();
        // The link notices asynchronously; poll briefly rather than sleep-and-hope.
        let mut err = None;
        for _ in 0..50 {
            match m.get("s1", "v").await {
                Err(RuntimeError::Unavailable(e)) => {
                    err = Some(e);
                    break;
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        assert!(err.is_some(), "a dead socket must read as Unavailable");
    }

    #[tokio::test]
    async fn gone_when_the_vendor_has_no_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(&tmp, published(&agent, "v"));
        let Err(err) = m.get("s1", "v").await else {
            panic!("a get must never provision")
        };
        assert!(
            matches!(err, RuntimeError::Gone(_)),
            "a live vendor with no runtime is terminal: {err:?}"
        );
    }

    #[tokio::test]
    async fn get_returns_a_client_after_create() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(&tmp, published(&agent, "v"));
        m.create("s1", "v", &session_spec("v"))
            .await
            .expect("create");
        m.get("s1", "v").await.expect("get after create");
        assert_eq!(
            agent.signals(),
            vec!["create:s1".to_string(), "get:s1".to_string()]
        );
    }

    #[tokio::test]
    async fn create_writes_the_capability_file_the_vendor_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(&tmp, published(&agent, "v"));
        m.create("s1", "v", &session_spec("v"))
            .await
            .expect("create");
        assert!(
            tmp.path()
                .join("sessions")
                .join("s1")
                .join("capabilities.json")
                .exists()
        );
        let sent = agent.last_create_request().expect("create request");
        assert!(
            sent.sandbox_capabilities.is_some(),
            "the vendor must receive the policy inline, not a server-local path"
        );
        assert_eq!(sent.workspaces, vec!["main".to_string()]);
    }

    #[tokio::test]
    async fn provider_is_a_thin_handle_over_the_same_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(&tmp, published(&agent, "v"));
        m.create("s1", "v", &session_spec("v"))
            .await
            .expect("create");
        let provider = m.provider("s1".to_string(), "v".to_string());
        provider.get().await.expect("provider get");
        assert_eq!(
            agent.signals(),
            vec!["create:s1".to_string(), "get:s1".to_string()]
        );
    }
}
