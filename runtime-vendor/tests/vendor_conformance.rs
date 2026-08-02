//! The vendor lifecycle contract, against the *real* agent loop.
//!
//! The server-side conformance tests drive `FakeRuntimeVendor`, whose command
//! loop is sequential — a blocked create genuinely blocks its socket read, so
//! those tests pass without any locking at all and say nothing about the agent
//! that ships. This one dials a real `RuntimeVendor::run` over a real
//! WebSocket, where every command dispatches on its own task, and holds it to
//! the same four promises:
//!
//! 1. `create` then `get` hands back a runtime.
//! 2. `get` without a create is `Gone` — it must never provision.
//! 3. `hibernate` is advisory; this agent declines, so `get` still works after.
//! 4. `get` during an in-flight `create` waits for it, rather than answering
//!    `Gone` for a runtime that is moments from existing.
//!
//! Only the sandbox process is doubled: the provider hands back a transport
//! instead of spawning `horsie-runtime`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use horsie_models::executor::RuntimeConfig;
use horsie_runtime_client::{MockTransport, RuntimeTransport};
use horsie_runtime_vendor::{
    ConnectedRuntimeRegistry, FixedWorkspaces, HealthStatus, RuntimeHandle, RuntimeProvider,
    RuntimeVendor,
};
use horsie_server::runtime_vendor::{RuntimeSpec, RuntimeVendorLink, VendorError, WorkspaceSpec};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// ── doubles ──────────────────────────────────────────────────────────────────

struct NoopHandle;

#[async_trait]
impl RuntimeHandle for NoopHandle {
    async fn stop(&self) -> Result<(), horsie_runtime_vendor::RuntimeError> {
        Ok(())
    }
    async fn health_check(&self) -> Result<HealthStatus, horsie_runtime_vendor::RuntimeError> {
        Ok(HealthStatus::Healthy)
    }
}

/// Stands in for spawning a sandbox: registers the transport the real runtime
/// would have dialed back with, optionally after waiting on a gate so a test can
/// hold a create open.
struct GatedProvider {
    connected: Arc<ConnectedRuntimeRegistry>,
    gate: Option<tokio::sync::watch::Receiver<bool>>,
}

#[async_trait]
impl RuntimeProvider for GatedProvider {
    async fn create(
        &self,
        id: &str,
        _config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, horsie_runtime_vendor::RuntimeError> {
        if let Some(gate) = &self.gate {
            let mut gate = gate.clone();
            while !*gate.borrow_and_update() {
                if gate.changed().await.is_err() {
                    break;
                }
            }
        }
        let transport: Arc<dyn RuntimeTransport> = Arc::new(MockTransport::ok(""));
        self.connected
            .register_transport(id.to_string(), transport)
            .await;
        Ok(Arc::new(NoopHandle))
    }
}

// ── harness ──────────────────────────────────────────────────────────────────

struct Agent {
    link: Arc<RuntimeVendorLink>,
    cancel: CancellationToken,
    /// Holds the workspace alive for the test's life.
    _dir: tempfile::TempDir,
}

impl Agent {
    /// The workspace name matches the agent's only configured directory.
    fn spec(&self) -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![WorkspaceSpec {
                name: "main".to_string(),
            }],
            provision: vec![],
            env: vec![],
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Bring up a real `RuntimeVendor` dialing a one-shot WebSocket endpoint, and
/// hand back the server-side link the session server would hold.
async fn start_agent(gate: Option<tokio::sync::watch::Receiver<bool>>) -> Agent {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (link_tx, link_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("the agent dials in");
        let ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("websocket upgrade");
        let link = RuntimeVendorLink::start(ws).await.expect("handshake");
        let _ = link_tx.send(link);
    });

    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let provider_connected = connected.clone();
    let mut workspaces = HashMap::new();
    workspaces.insert("main".to_string(), dir.path().to_path_buf());

    let agent = RuntimeVendor::new(
        "conformance".to_string(),
        true,
        Arc::new(move |_id: &str, _caps: Option<PathBuf>| {
            Arc::new(GatedProvider {
                connected: provider_connected.clone(),
                gate: gate.clone(),
            })
        }),
        connected,
        Arc::new(FixedWorkspaces::new(workspaces)),
        state_dir,
    );
    let cancel = CancellationToken::new();
    let url = format!("ws://{addr}/api/vendor/connect");
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let _ = agent.run(&url, cancel).await;
        }
    });

    let link = tokio::time::timeout(Duration::from_secs(5), link_rx)
        .await
        .expect("the agent must connect")
        .expect("link channel");
    Agent {
        link,
        cancel,
        _dir: dir,
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_after_create_returns_a_runtime() {
    let agent = start_agent(None).await;
    agent
        .link
        .create("rt-1", &agent.spec())
        .await
        .expect("create");
    agent.link.get("rt-1").await.expect("get after create");
}

#[tokio::test]
async fn get_without_create_is_gone_and_provisions_nothing() {
    let agent = start_agent(None).await;
    let err = agent
        .link
        .get("rt-unknown")
        .await
        .expect_err("a get must never provision");
    assert!(
        matches!(err, VendorError::Gone(_)),
        "an absent runtime is Gone, not Unavailable: {err:?}"
    );
    // And having said so, it still holds nothing: a second get repeats it
    // rather than finding something the first one created.
    assert!(matches!(
        agent.link.get("rt-unknown").await,
        Err(VendorError::Gone(_))
    ));
}

#[tokio::test]
async fn hibernate_is_advisory_and_this_agent_declines_it() {
    let agent = start_agent(None).await;
    agent
        .link
        .create("rt-1", &agent.spec())
        .await
        .expect("create");
    agent.link.hibernate("rt-1").await;
    // A process cannot be suspended and re-entered, so this agent keeps the
    // runtime — and the session it belongs to is still resumable.
    agent
        .link
        .get("rt-1")
        .await
        .expect("get after an advisory hibernate");
}

/// The promise `lifecycle_locks` exists to keep. The real agent dispatches every
/// command on its own task, so without the per-id lock this `get` races ahead of
/// the create it belongs to and answers `Gone` for a runtime that is moments
/// from existing — which the session turns into a terminal, unrecoverable state.
#[tokio::test]
async fn get_during_an_in_flight_create_waits_for_it() {
    let (open, gate) = tokio::sync::watch::channel(false);
    let agent = start_agent(Some(gate)).await;

    let creating = {
        let link = agent.link.clone();
        let spec = agent.spec();
        tokio::spawn(async move { link.create("rt-1", &spec).await })
    };
    // Let the create reach the provider and park there.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let getting = {
        let link = agent.link.clone();
        tokio::spawn(async move { link.get("rt-1").await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !getting.is_finished(),
        "the get must wait for the in-flight create, not answer Gone"
    );

    let _ = open.send(true);
    creating.await.expect("join").expect("create");
    getting
        .await
        .expect("join")
        .expect("the get resolves once the create lands");
}
