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
    RuntimeSpec, RuntimeVendorLink, RuntimeVendorTransport, VendorError, WorkspaceSpec,
};
use crate::sessions::spec::{SessionSpec, SharedVendors};
use horsie_runtime_client::RuntimeClient;
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

    /// Assemble the vendor-facing spec.
    ///
    /// Re-assembled on every create rather than cached: the GitHub token and
    /// the plugin token are short-lived, and a stale one is worse than none.
    async fn runtime_spec(
        &self,
        session: &str,
        spec: &SessionSpec,
    ) -> Result<RuntimeSpec, RuntimeError> {
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
        // token; the runtime reads both from its environment at startup.
        //
        // Every vendor participates, including one that cannot provision a
        // workspace. Bundles are not a workspace: the runtime fetches them over
        // its own outbound connection into its own plugins dir, which it can do
        // over a directory it did not create. `horsie connect` announces
        // `supports_provisioning: false` yet already wires `with_bundles`, so
        // gating here was the one thing keeping skills off the most common
        // self-hosted vendor.
        //
        // The runtime resolves the overlap with a host `--plugins-dir` library:
        // fetched bundles win, the host library is the fallback. So selecting
        // bundles replaces the host library for that session, and selecting
        // none leaves it in place.
        if let Some(prov) = self.deps.plugins.as_ref() {
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
        let rt_spec = self.runtime_spec(session, spec).await?;
        link.create(session, &rt_spec)
            .await
            .map_err(|e: VendorError| match e {
                VendorError::Gone(m) => RuntimeError::Gone(m),
                VendorError::Unavailable(m) => RuntimeError::Unavailable(m),
                VendorError::Provision(m) => RuntimeError::Provision(m),
            })
    }

    /// Hand back a client for this session's runtime, resuming it if the
    /// vendor hibernated it. Never provisions.
    ///
    /// The client is bound to the vendor's *name*, not to the link this call
    /// happened to resolve. A caller holds it for a whole run — the toolbox an
    /// agent loop executes against is built once — and a vendor agent that
    /// reconnects mid-run comes back on a different link. Binding to the name
    /// means the next tool call finds it; binding to the link meant every tool
    /// call for the rest of that turn failed on a dead socket.
    pub async fn get(&self, session: &str, vendor: &str) -> Result<RuntimeClient, RuntimeError> {
        let link = self.vendor(vendor)?;
        link.get(session).await.map_err(|e| match e {
            VendorError::Gone(m) => RuntimeError::Gone(m),
            VendorError::Unavailable(m) => RuntimeError::Unavailable(m),
            VendorError::Provision(m) => RuntimeError::Provision(m),
        })?;
        Ok(self.client(session, vendor))
    }

    /// A client for a runtime the vendor has just confirmed.
    ///
    /// The runtime's own id doubles as its main agent's identity: the server
    /// passes the session id as `runtime_id`, and that is also what the agent
    /// journal is keyed by (`agent/<session-uuid>`). A subagent sharing this
    /// runtime derives its own handle with `RuntimeClient::with_agent_id`.
    fn client(&self, session: &str, vendor: &str) -> RuntimeClient {
        let transport = RuntimeVendorTransport::new(
            self.deps.vendors.clone(),
            vendor.to_string(),
            session.to_string(),
        );
        RuntimeClient::from_arc(Arc::new(transport), session)
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

/// A `RuntimeManager` over the vendor map the deps carry.
/// Test-only: production builds it once in `main`.
#[cfg(test)]
pub(crate) fn test_runtime_manager(
    vendors: &crate::sessions::spec::SharedVendors,
) -> std::sync::Arc<crate::runtime_manager::RuntimeManager> {
    std::sync::Arc::new(crate::runtime_manager::RuntimeManager::new(
        crate::runtime_manager::RuntimeDeps {
            vendors: vendors.clone(),
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
                max_concurrent_subagents: None,
            },
            workspaces: vec![WorkspaceDef {
                name: "main".into(),
            }],
            provision: vec![],
            vendor: vendor.into(),
            plugins: vec![],
            origin: crate::sessions::spec::SessionOrigin::User,
            workflow: None,
        }
    }

    fn manager(vendors: SharedVendors) -> Arc<RuntimeManager> {
        Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors,
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
        let m = manager(Arc::new(RwLock::new(HashMap::new())));
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
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
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
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
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
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
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
    async fn create_sends_the_vendor_workspace_names_not_paths() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        m.create("s1", "v", &session_spec("v"))
            .await
            .expect("create");
        let sent = agent.last_create_request().expect("create request");
        assert_eq!(sent.workspaces, vec!["main".to_string()]);
    }

    /// Resolves any name to a hash of itself, so a test can assert on the
    /// manifest without a plugin store.
    struct FakeProvisioner;

    #[async_trait::async_trait]
    impl crate::plugins::PluginProvisioner for FakeProvisioner {
        async fn resolve(
            &self,
            names: &[String],
        ) -> Result<Vec<crate::plugins::PluginArtifactRef>, String> {
            Ok(names
                .iter()
                .map(|n| crate::plugins::PluginArtifactRef {
                    name: n.clone(),
                    hash: format!("hash-of-{n}"),
                })
                .collect())
        }

        fn mint_token(&self, session_id: &str, _hashes: &[String]) -> String {
            format!("token-for-{session_id}")
        }

        async fn default_names(&self) -> Vec<String> {
            vec![]
        }
    }

    /// A vendor that cannot provision a workspace still gets the bundle
    /// manifest. Bundles are not a workspace: the runtime fetches them over its
    /// own outbound connection into its own plugins dir, which works over a
    /// directory it did not create. `horsie connect` announces
    /// `supports_provisioning: false` and is exactly this case — gating here is
    /// what used to keep skills off the most common self-hosted vendor.
    #[tokio::test]
    async fn a_vendor_that_cannot_provision_still_receives_the_bundle_manifest() {
        let agent = FakeRuntimeVendor::builder("v")
            .supports_provisioning(false)
            .serve_in_process()
            .await
            .unwrap();
        let m = Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors: published(&agent, "v"),
            github_tokens: None,
            plugins: Some(Arc::new(FakeProvisioner)),
        }));
        let mut spec = session_spec("v");
        spec.plugins = vec!["superpowers".to_string()];
        m.create("s1", "v", &spec).await.expect("create");

        let sent = agent.last_create_request().expect("create request");
        let env = |name: &str| {
            sent.env
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.value.clone())
        };
        let manifest = env(horsie_models::ENV_PLUGIN_MANIFEST)
            .expect("a non-provisioning vendor must still be sent the manifest");
        assert!(
            manifest.contains("hash-of-superpowers"),
            "the manifest names the selected bundle: {manifest}"
        );
        assert_eq!(
            env(horsie_models::ENV_PLUGINS_TOKEN).as_deref(),
            Some("token-for-s1")
        );
    }

    /// Mints a distinct token every call, so a test can prove credentials are
    /// never reused across a session's lifetime.
    struct CountingMinter {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingMinter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::github::GithubTokenMinter for CountingMinter {
        async fn mint_for(&self, _repo_urls: &[String]) -> Result<Option<String>, String> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(format!("token-{n}")))
        }
    }

    #[tokio::test]
    async fn create_assembles_env_fresh_each_time() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let minter = CountingMinter::new();
        let m = Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors: published(&agent, "v"),
            github_tokens: Some(minter.clone() as Arc<dyn crate::github::GithubTokenMinter>),
            plugins: None,
        }));
        let mut spec = session_spec("v");
        spec.provision
            .push(crate::sessions::spec::ProvisionStepSpec {
                name: "checkout".into(),
                uses: "git_checkout".into(),
                with: vec![("url".into(), "https://example.com/repo.git".into())],
            });

        m.create("s1", "v", &spec).await.expect("first create");
        let first_token = agent
            .last_create_request()
            .expect("create request")
            .env
            .iter()
            .find(|e| e.name == horsie_models::ENV_GITHUB_TOKEN)
            .map(|e| e.value.clone())
            .expect("a minted token must be on the wire");

        m.create("s1", "v", &spec).await.expect("second create");
        let second_token = agent
            .last_create_request()
            .expect("create request")
            .env
            .iter()
            .find(|e| e.name == horsie_models::ENV_GITHUB_TOKEN)
            .map(|e| e.value.clone())
            .expect("a minted token must be on the wire");

        assert_ne!(
            first_token, second_token,
            "credentials are short-lived and must be re-minted on every create, never cached"
        );
        assert_eq!(minter.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn provider_is_a_thin_handle_over_the_same_calls() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
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
