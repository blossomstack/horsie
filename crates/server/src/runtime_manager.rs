//! Runtime lifecycle, owned server-side and kept off every actor mailbox.
//!
//! The session and agent actors do not know how a runtime comes to exist, how
//! it is resumed, or how it goes away. They ask for a client and get one, or
//! they get one of two errors that mean genuinely different things
//! ([`RuntimeError::Unavailable`] is retryable; [`RuntimeError::Gone`] is
//! terminal). Everything else — resolving the vendor, assembling the spec,
//! minting short-lived credentials — happens here.
//!
//! **Created once.** [`RuntimeManager::create`] has exactly one caller — the
//! session actor that owns the runtime — and [`RuntimeManager::get`] can never
//! provision, so no later code path can silently rebuild a workspace the user
//! believes still exists.
//!
//! **A create does not wait; an acquisition does.** Nothing here knows a create
//! is in flight — that belongs to the session, which journals the attempt and
//! refuses to start a turn until it has an answer, a wait that survives the
//! process dying mid-create where one held in a map beside this manager could
//! not. [`RuntimeManager::get`] is the other case: a vendor whose substrate
//! boots a machine answers `Starting` and reports the outcome on a progress
//! sink, so somebody has to read that sink, and the caller asking for a client
//! is the only one who needs the answer.

use crate::runtime_vendor::RuntimeVendor;
use crate::runtime_vendor::{RuntimeSpec, RuntimeVendorError, WorkspaceSpec};
use crate::sessions::spec::{RuntimeVendorMap, SessionSpec};
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
    pub vendors: RuntimeVendorMap,
    pub github_tokens: Option<Arc<dyn crate::github::GithubTokenMinter>>,
    pub plugins: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
}

/// How many progress reports may queue before the oldest are dropped.
///
/// Dropping is correct: progress is advisory and the call's return value is the
/// outcome, so a consumer falling behind must never stall a provision.
const PROGRESS_BUFFER: usize = 32;

/// How long an acquisition waits for a runtime to become reachable.
///
/// Above every vendor's own ready window on purpose, so the vendor is always
/// the one that gives up first and says why. This is the backstop for a vendor
/// that drops its sink without ever reporting an outcome — without it such a
/// vendor would park a turn forever.
const ACQUIRE_WINDOW: std::time::Duration = std::time::Duration::from_secs(960);

pub struct RuntimeManager {
    deps: RuntimeDeps,
}

impl RuntimeManager {
    #[must_use]
    pub fn new(deps: RuntimeDeps) -> Self {
        Self { deps }
    }

    fn vendor(&self, vendor: &str) -> Result<Arc<dyn RuntimeVendor>, RuntimeError> {
        let links =
            self.deps.vendors.read().map_err(|_| {
                RuntimeError::Unavailable("vendor registry lock poisoned".to_string())
            })?;
        let link = links.get(vendor).cloned().ok_or_else(|| {
            RuntimeError::Unavailable(format!("unknown runtime vendor '{vendor}'"))
        })?;
        if !link.is_reachable() {
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
            // The environment's variables first; the server pushes its own
            // (the minted GitHub token, below) after. A name that would shadow
            // one cannot reach here — the environment service refuses it at
            // save.
            env: spec
                .env_vars
                .iter()
                .map(|v| horsie_models::executor::EnvVar {
                    name: v.name.clone(),
                    value: v.value.clone(),
                })
                .collect(),
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
        // Not awaited to `Ready`, unlike an acquisition. A create's job is to
        // get the substrate to accept the runtime; the session journals that it
        // happened and the first `get` is what waits for it to come up — a wait
        // that survives this process dying, which one held here would not.
        let (progress, _rx) = tokio::sync::mpsc::channel(PROGRESS_BUFFER);
        link.create(session, &rt_spec.to_wire(), progress)
            .await
            .map_err(Self::vendor_error)?;
        Ok(())
    }

    /// Hand back a client for this session's runtime, resuming it if the
    /// vendor hibernated it. Never provisions.
    ///
    /// The client is bound to the vendor's *name*, not to the link this call
    /// happened to resolve. A caller holds it for a whole run — the toolbox an
    /// agent loop executes against is built once — and a vendor process that
    /// reconnects mid-run comes back on a different link. Binding to the name
    /// means the next tool call finds it; binding to the link meant every tool
    /// call for the rest of that turn failed on a dead socket.
    pub async fn get(
        &self,
        session: &str,
        vendor: &str,
        spec: &SessionSpec,
    ) -> Result<RuntimeClient, RuntimeError> {
        let link = self.vendor(vendor)?;
        // The receiver is held for the whole acquisition, not dropped on the
        // way out. A vendor whose substrate has to boot a machine answers
        // `Starting` and finishes on this sink — so dropping it closed the one
        // channel the eventual `Ready` had to arrive on, and every Fly or velos
        // acquisition failed as "not reachable yet" no matter how long the
        // runtime had been up.
        let (progress, mut rx) = tokio::sync::mpsc::channel(PROGRESS_BUFFER);
        let rt_spec = self.runtime_spec(session, spec).await?;
        let first = link
            .get(session, &rt_spec.to_wire(), progress)
            .await
            .map_err(Self::vendor_error)?;
        let handle = Self::await_ready(session, first, &mut rx).await?;
        Ok(Self::client(session, handle))
    }

    fn vendor_error(e: RuntimeVendorError) -> RuntimeError {
        match e {
            RuntimeVendorError::Gone(m) => RuntimeError::Gone(m),
            RuntimeVendorError::Unavailable(m) => RuntimeError::Unavailable(m),
            RuntimeVendorError::Provision(m) => RuntimeError::Provision(m),
        }
    }

    /// Follow an acquisition from its first observation to a runtime that can
    /// be talked to.
    ///
    /// The vendor contract makes this a plain fold: the return value *is* the
    /// first event, and every later one arrives on the sink in order, so there
    /// is nothing to reconcile — only a state to walk until it settles.
    ///
    /// Events for another runtime are ignored rather than trusted: one account
    /// has one sink, and a vendor is free to report on anything it owns.
    async fn await_ready(
        session: &str,
        first: horsie_runtime_vendor::RuntimeProgress,
        rx: &mut tokio::sync::mpsc::Receiver<horsie_runtime_vendor::RuntimeEvent>,
    ) -> Result<Arc<dyn crate::runtime_vendor::RuntimeHandle>, RuntimeError> {
        use horsie_runtime_vendor::RuntimeProgress as P;
        let deadline = tokio::time::Instant::now() + ACQUIRE_WINDOW;
        let mut progress = first;
        loop {
            match progress {
                P::Ready(handle) => return Ok(handle),
                // Terminal, and the reason travels: a session whose runtime is
                // gone has to be able to say so rather than retry forever.
                P::Gone { reason } => return Err(RuntimeError::Gone(reason)),
                // Not terminal. A vendor that reports a runtime stopped during
                // an acquisition is one that could not revive it this time.
                P::Stopped | P::Stopping => {
                    return Err(RuntimeError::Unavailable(format!(
                        "runtime '{session}' went down during the acquisition"
                    )));
                }
                P::Requested | P::Starting { .. } | P::Provisioning { .. } => {}
            }
            let event = tokio::time::timeout_at(deadline, rx.recv()).await;
            progress = match event {
                Ok(Some(event)) if event.runtime_id == session => event.progress,
                Ok(Some(_)) => continue,
                // The vendor dropped the sink without ever reporting an
                // outcome. Retryable rather than terminal: it says nothing
                // about the runtime, only about the vendor.
                Ok(None) => {
                    return Err(RuntimeError::Unavailable(format!(
                        "the vendor stopped reporting on runtime '{session}'"
                    )));
                }
                Err(_) => {
                    return Err(RuntimeError::Unavailable(format!(
                        "runtime '{session}' was not reachable within the acquisition window"
                    )));
                }
            };
        }
    }

    /// A client over the handle the vendor just returned.
    ///
    /// The handle is bound to the vendor's *name*, not to the link that
    /// answered this call: a caller holds the client for a whole run — the
    /// toolbox an agent loop executes against is built once — and a vendor
    /// process that reconnects mid-run comes back on a different link. Binding
    /// to the name means the next tool call finds it; binding to the link meant
    /// every call for the rest of that turn failed on a dead socket (#187).
    ///
    /// The runtime's own id doubles as its main agent's identity: the server
    /// passes the session id as `runtime_id`, and that is also what the agent
    /// journal is keyed by (`agent/<session-uuid>`). A subagent sharing this
    /// runtime derives its own handle with `RuntimeClient::with_agent_id`.
    fn client(
        session: &str,
        handle: Arc<dyn crate::runtime_vendor::RuntimeHandle>,
    ) -> RuntimeClient {
        RuntimeClient::from_arc(
            Arc::new(horsie_runtime_vendor::RuntimeHandleTransport(handle)),
            session,
        )
    }

    /// Advisory: the session is going cold. Best effort — a vendor that is not
    /// there simply misses the hint, and nothing about the session changes.
    pub async fn hibernate(&self, session: &str, vendor: &str) {
        if let Ok(link) = self.vendor(vendor) {
            let (progress, _rx) = tokio::sync::mpsc::channel(PROGRESS_BUFFER);
            let _ = link.hibernate(session, progress).await;
        }
    }

    /// The session was deleted; the vendor decides the runtime's fate.
    pub async fn delete(&self, session: &str, vendor: &str) {
        if let Ok(link) = self.vendor(vendor) {
            let (progress, _rx) = tokio::sync::mpsc::channel(PROGRESS_BUFFER);
            let _ = link.delete(session, progress).await;
        }
    }

    /// A cheap handle bound to one session, for whoever needs to execute.
    #[must_use]
    pub fn provider(
        self: &Arc<Self>,
        session: String,
        vendor: String,
        spec: SessionSpec,
    ) -> RuntimeClientProvider {
        RuntimeClientProvider {
            manager: self.clone(),
            session,
            vendor,
            spec,
        }
    }
}

/// The agent's view of the runtime: one method, no lifecycle.
#[derive(Clone)]
pub struct RuntimeClientProvider {
    manager: Arc<RuntimeManager>,
    session: String,
    vendor: String,
    /// Held so an acquisition can carry the spec: the server is the only
    /// durable holder of it, and a vendor keeps no copy on disk.
    spec: SessionSpec,
}

impl RuntimeClientProvider {
    /// A working client for this session's runtime, resumed if need be.
    pub async fn get(&self) -> Result<RuntimeClient, RuntimeError> {
        self.manager
            .get(&self.session, &self.vendor, &self.spec)
            .await
    }
}

/// A `RuntimeManager` over the vendor map the deps carry.
/// Test-only: production builds it once in `main`.
#[cfg(test)]
pub(crate) fn test_runtime_manager(
    vendors: &crate::sessions::spec::RuntimeVendorMap,
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
            environment: None,
            env_vars: vec![],
        }
    }

    fn manager(vendors: RuntimeVendorMap) -> Arc<RuntimeManager> {
        Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors,
            github_tokens: None,
            plugins: None,
        }))
    }

    fn published(agent: &FakeRuntimeVendor, name: &str) -> RuntimeVendorMap {
        let mut map = HashMap::new();
        map.insert(
            name.to_string(),
            agent.link() as Arc<dyn crate::runtime_vendor::RuntimeVendor>,
        );
        Arc::new(RwLock::new(map))
    }

    #[tokio::test]
    async fn unavailable_when_the_vendor_name_is_not_registered() {
        let m = manager(Arc::new(RwLock::new(HashMap::new())));
        let Err(err) = m.get("s1", "nope", &SessionSpec::for_vendor("v")).await else {
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
            match m.get("s1", "v", &SessionSpec::for_vendor("v")).await {
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
        let Err(err) = m.get("s1", "v", &SessionSpec::for_vendor("v")).await else {
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
        m.get("s1", "v", &SessionSpec::for_vendor("v"))
            .await
            .expect("get after create");
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

    #[tokio::test]
    async fn the_environments_variables_reach_the_vendor() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        let mut spec = session_spec("v");
        spec.env_vars.push(crate::sessions::spec::EnvVarSpec {
            name: "RUST_LOG".into(),
            value: "debug".into(),
        });
        m.create("s1", "v", &spec).await.expect("create");
        let sent = agent.last_create_request().expect("create request");
        assert_eq!(
            sent.env
                .iter()
                .find(|e| e.name == "RUST_LOG")
                .map(|e| e.value.as_str()),
            Some("debug")
        );
    }

    /// Resolves any name to a hash of itself, so a test can assert on the
    /// manifest without a plugin store.
    struct FakeProvisioner;

    #[async_trait::async_trait]
    impl crate::plugins::PluginProvisioner for FakeProvisioner {
        async fn catalog(
            &self,
            _names: &[String],
        ) -> Vec<horsie_support::plugin::catalog::CatalogEntry> {
            Vec::new()
        }

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

    /// A substrate that has to boot something: `Starting` first, the outcome on
    /// the sink later. Every cloud vendor in the tree behaves this way, and no
    /// websocket-backed double can stand in for one — a `horsie connect` link
    /// only ever answers once its runtime is already up.
    struct BootingVendor {
        outcome: std::sync::Mutex<Option<horsie_runtime_vendor::RuntimeProgress>>,
    }

    impl BootingVendor {
        fn with(outcome: horsie_runtime_vendor::RuntimeProgress) -> Arc<Self> {
            Arc::new(Self {
                outcome: std::sync::Mutex::new(Some(outcome)),
            })
        }

        /// Never reports an outcome at all, the way a vendor whose background
        /// task died does.
        fn silent() -> Arc<Self> {
            Arc::new(Self {
                outcome: std::sync::Mutex::new(None),
            })
        }

        fn ready() -> Arc<Self> {
            Self::with(horsie_runtime_vendor::RuntimeProgress::Ready(Arc::new(
                StubHandle,
            )))
        }
    }

    #[derive(Debug)]
    struct StubHandle;

    #[async_trait::async_trait]
    impl crate::runtime_vendor::RuntimeHandle for StubHandle {
        fn id(&self) -> &str {
            "s1"
        }
        async fn relay(
            &self,
            _: horsie_models::runtime::RuntimeInboundMessage,
        ) -> Result<
            horsie_models::runtime::RuntimeOutboundMessage,
            horsie_runtime_client::TransportError,
        > {
            Err(horsie_runtime_client::TransportError::Disconnected)
        }
        async fn relay_oneway(
            &self,
            _: horsie_models::runtime::RuntimeInboundMessage,
        ) -> Result<(), horsie_runtime_client::TransportError> {
            Ok(())
        }
        async fn closed(&self) {
            std::future::pending::<()>().await;
        }
    }

    #[async_trait::async_trait]
    impl crate::runtime_vendor::RuntimeVendor for BootingVendor {
        fn name(&self) -> &str {
            "booting"
        }
        fn capabilities(&self) -> horsie_models::runtime_vendor::RuntimeVendorCapabilities {
            horsie_models::runtime_vendor::RuntimeVendorCapabilities {
                supports_provisioning: true,
            }
        }
        async fn create(
            &self,
            _: &str,
            _: &horsie_models::runtime_vendor::RuntimeSpec,
            _: horsie_runtime_vendor::RuntimeProgressSink,
        ) -> Result<horsie_runtime_vendor::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_vendor::RuntimeProgress::Starting {
                detail: "booting".into(),
            })
        }
        async fn get(
            &self,
            runtime_id: &str,
            _spec: &horsie_models::runtime_vendor::RuntimeSpec,
            progress: horsie_runtime_vendor::RuntimeProgressSink,
        ) -> Result<horsie_runtime_vendor::RuntimeProgress, RuntimeVendorError> {
            let outcome = self.outcome.lock().unwrap().take();
            let id = runtime_id.to_string();
            // After the return value is built, per the ordering rule.
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                if let Some(progress_step) = outcome {
                    let _ = progress
                        .send(horsie_runtime_vendor::RuntimeEvent {
                            runtime_id: id,
                            progress: progress_step,
                        })
                        .await;
                }
            });
            Ok(horsie_runtime_vendor::RuntimeProgress::Starting {
                detail: "the machine is up; waiting for it to dial back".into(),
            })
        }
        async fn hibernate(
            &self,
            _: &str,
            _: horsie_runtime_vendor::RuntimeProgressSink,
        ) -> Result<horsie_runtime_vendor::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_vendor::RuntimeProgress::Stopped)
        }
        async fn delete(
            &self,
            _: &str,
            _: horsie_runtime_vendor::RuntimeProgressSink,
        ) -> Result<horsie_runtime_vendor::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_vendor::RuntimeProgress::Gone {
                reason: "deleted".into(),
            })
        }
    }

    fn published_vendor(vendor: Arc<dyn crate::runtime_vendor::RuntimeVendor>) -> RuntimeVendorMap {
        let mut map = HashMap::new();
        map.insert("v".to_string(), vendor);
        Arc::new(RwLock::new(map))
    }

    /// The failure that made every cloud vendor unusable. A vendor whose
    /// substrate boots a machine answers `Starting` and reports `Ready` on the
    /// sink — and the sink's receiver was dropped on the way out of this call,
    /// so the `Ready` went into a closed channel and every acquisition failed
    /// as "not reachable yet", however long the runtime had been up.
    #[tokio::test]
    async fn an_acquisition_follows_a_booting_runtime_to_ready() {
        let vendor = BootingVendor::ready();
        let m = manager(published_vendor(vendor));
        m.get("s1", "v", &SessionSpec::for_vendor("v"))
            .await
            .expect("a runtime that comes up on the sink must be handed back");
    }

    /// The other half of the fold: a vendor that gives up says so, and says it
    /// terminally, so the session stops retrying a runtime that is not coming
    /// back.
    #[tokio::test]
    async fn an_acquisition_that_ends_gone_is_terminal() {
        let vendor = BootingVendor::with(horsie_runtime_vendor::RuntimeProgress::Gone {
            reason: "the machine never dialed back".into(),
        });
        let m = manager(published_vendor(vendor));
        let Err(err) = m.get("s1", "v", &SessionSpec::for_vendor("v")).await else {
            panic!("a runtime reported gone must not yield a client")
        };
        assert!(
            matches!(&err, RuntimeError::Gone(m) if m.contains("never dialed back")),
            "{err:?}"
        );
    }

    /// A vendor that drops its sink without ever reporting is retryable, not
    /// terminal: it says nothing about the runtime, only about the vendor.
    #[tokio::test]
    async fn a_vendor_that_stops_reporting_leaves_the_session_recoverable() {
        let m = manager(published_vendor(BootingVendor::silent()));
        let Err(err) = m.get("s1", "v", &SessionSpec::for_vendor("v")).await else {
            panic!("a silent vendor must not yield a client")
        };
        assert!(matches!(err, RuntimeError::Unavailable(_)), "{err:?}");
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
        let provider = m.provider(
            "s1".to_string(),
            "v".to_string(),
            SessionSpec::for_vendor("v"),
        );
        provider.get().await.expect("provider get");
        assert_eq!(
            agent.signals(),
            vec!["create:s1".to_string(), "get:s1".to_string()]
        );
    }
}
