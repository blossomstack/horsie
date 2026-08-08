//! The vendor lifecycle contract, against the *real* agent loop.
//!
//! The server-side conformance tests drive `FakeRuntimeVendor`, whose command
//! loop is sequential — a blocked create genuinely blocks its socket read, so
//! those tests pass without any locking at all and say nothing about the agent
//! that ships. This one dials a real `RuntimeVendorClient::run` over a real
//! WebSocket, where every command dispatches on its own task, and holds it to
//! the same promises:
//!
//! 1. `create` then `get` hands back a runtime.
//! 2. `get` without a create is `Gone` — it must never provision.
//! 3. `hibernate` is advisory: an agent that cannot rebuild the runtime
//!    declines it, and one that can frees the process and rebuilds on the next
//!    `get`.
//! 4. `get` during an in-flight `create` waits for it, rather than answering
//!    `Gone` for a runtime that is moments from existing.
//! 5. A respawnable agent's runtimes outlive the agent process; a
//!    non-respawnable one's do not, and saying `Gone` is the safe answer there.
//!
//! Only the sandbox process is doubled: the provider hands back a transport
//! instead of spawning `horsie-runtime`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use horsie_models::executor::RuntimeConfig;
use horsie_runtime_client::{MockTransport, RuntimeTransport};
use horsie_runtime_vendor::{
    AgentExit, ConnectedRuntimeRegistry, CredentialProvider, FixedWorkspaces, HealthStatus,
    RuntimeHandle, RuntimeProvider, RuntimeVendorClient,
};
use horsie_server::auth::Principal;
use horsie_server::runtime_vendor::{
    RuntimeSpec, RuntimeVendorError, WebsocketRuntimeVendor, WorkspaceSpec,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// ── doubles ──────────────────────────────────────────────────────────────────

/// Stands in for a spawned process. Stopping it deregisters the transport, as
/// `ProcessRuntimeHandle::stop` does — without that the agent would still find
/// a live transport for a runtime it had just killed, and no test could tell a
/// working hibernate from a no-op one.
struct StoppableHandle {
    connected: Arc<ConnectedRuntimeRegistry>,
    runtime_id: String,
}

#[async_trait]
impl RuntimeHandle for StoppableHandle {
    async fn stop(&self) -> Result<(), horsie_runtime_vendor::RuntimeError> {
        self.connected.remove(&self.runtime_id).await;
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
        Ok(Arc::new(StoppableHandle {
            connected: self.connected.clone(),
            runtime_id: id.to_string(),
        }))
    }
}

// ── harness ──────────────────────────────────────────────────────────────────

struct Agent {
    link: Arc<WebsocketRuntimeVendor>,
    cancel: CancellationToken,
    /// The transports this incarnation's runtimes dialed back on, so a test can
    /// tell a live runtime from a stopped one.
    connected: Arc<ConnectedRuntimeRegistry>,
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

    async fn is_live(&self, runtime_id: &str) -> bool {
        self.connected.runtime_transport(runtime_id).await.is_some()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The machine an agent runs on: the workspace it serves and the state
/// directory it records runtimes in.
///
/// Separate from [`Agent`] because it outlives one. That is the whole subject
/// of the restart tests — the process goes away, the directories do not.
struct Machine {
    dir: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    /// Bring up a real `RuntimeVendorClient` dialing a one-shot WebSocket endpoint,
    /// and hand back the server-side link the session server would hold.
    async fn start(
        &self,
        gate: Option<tokio::sync::watch::Receiver<bool>>,
        respawnable: bool,
    ) -> Agent {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let (link_tx, link_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("the agent dials in");
            let ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("websocket upgrade");
            // Auth-disabled shape: every principal is anonymous.
            let link = WebsocketRuntimeVendor::start(ws, Principal::Anonymous)
                .await
                .expect("handshake");
            // The agent waits to be told it is published before it serves, so a
            // stand-in server has to answer the handshake like the real one.
            link.confirm_registration()
                .await
                .expect("acknowledge the registration");
            let _ = link_tx.send(link);
        });

        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        let provider_connected = connected.clone();
        let mut workspaces = HashMap::new();
        workspaces.insert("main".to_string(), self.dir.path().to_path_buf());

        let agent = RuntimeVendorClient::new(
            "conformance".to_string(),
            true,
            Arc::new(move |_id: &str, _caps: Option<PathBuf>| {
                Arc::new(GatedProvider {
                    connected: provider_connected.clone(),
                    gate: gate.clone(),
                })
            }),
            connected.clone(),
            Arc::new(FixedWorkspaces::new(workspaces)),
            self.state_dir(),
        )
        .with_respawnable_runtimes(respawnable);
        let cancel = CancellationToken::new();
        let url = format!("ws://{addr}/api/vendor/connect");
        tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let credential: CredentialProvider = Arc::new(|| Box::pin(async { Ok(None) }));
                let _ = agent.run(&url, credential, cancel).await;
            }
        });

        let link = tokio::time::timeout(Duration::from_secs(5), link_rx)
            .await
            .expect("the agent must connect")
            .expect("link channel");
        Agent {
            link,
            cancel,
            connected,
        }
    }
}

/// A machine plus its first agent, for the tests that never restart one.
async fn start_agent(gate: Option<tokio::sync::watch::Receiver<bool>>) -> (Machine, Agent) {
    let machine = Machine::new();
    let agent = machine.start(gate, false).await;
    (machine, agent)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_after_create_returns_a_runtime() {
    let (_machine, agent) = start_agent(None).await;
    agent
        .link
        .create("rt-1", &agent.spec())
        .await
        .expect("create");
    agent.link.get("rt-1").await.expect("get after create");
}

#[tokio::test]
async fn get_without_create_is_gone_and_provisions_nothing() {
    let (_machine, agent) = start_agent(None).await;
    let err = agent
        .link
        .get("rt-unknown")
        .await
        .expect_err("a get must never provision");
    assert!(
        matches!(err, RuntimeVendorError::Gone(_)),
        "an absent runtime is Gone, not Unavailable: {err:?}"
    );
    // And having said so, it still holds nothing: a second get repeats it
    // rather than finding something the first one created.
    assert!(matches!(
        agent.link.get("rt-unknown").await,
        Err(RuntimeVendorError::Gone(_))
    ));
}

#[tokio::test]
async fn hibernate_is_advisory_and_this_agent_declines_it() {
    let (_machine, agent) = start_agent(None).await;
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

// ── credentials ──────────────────────────────────────────────────────────────

/// A vendor with nothing configured, for the credential tests: they never get
/// as far as needing a provider or a workspace.
fn bare_vendor() -> RuntimeVendorClient {
    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    RuntimeVendorClient::new(
        "creds".to_string(),
        false,
        Arc::new(move |_id: &str, _caps: Option<PathBuf>| {
            Arc::new(GatedProvider {
                connected: Arc::new(ConnectedRuntimeRegistry::new()),
                gate: None,
            })
        }),
        connected,
        Arc::new(FixedWorkspaces::new(HashMap::new())),
        PathBuf::from("/nonexistent"),
    )
    // Milliseconds, so a test can watch several attempts go by.
    .with_backoff(horsie_runtime_vendor::Backoff::new(
        Duration::from_millis(5),
        Duration::from_millis(5),
    ))
}

/// Port 1 on loopback: nothing listens, so every dial fails and the loop keeps
/// coming back for another credential.
const UNDIALABLE: &str = "ws://127.0.0.1:1/api/vendor/connect";

/// The 401 loop, in one assertion. The agent used to capture one token at
/// startup and present it on every reconnect for the life of the process, so a
/// token that expired mid-link was retried forever. Every attempt must ask
/// again.
#[tokio::test]
async fn the_credential_is_resolved_on_every_attempt() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let credential: CredentialProvider = Arc::new(move || {
        let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { Ok(Some(format!("token-{n}"))) })
    });

    let cancel = CancellationToken::new();
    let running = tokio::spawn({
        let cancel = cancel.clone();
        async move { bare_vendor().run(UNDIALABLE, credential, cancel).await }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    cancel.cancel();
    running
        .await
        .expect("join")
        .expect("cancellation is not an error");

    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) > 1,
        "a reused token would have been resolved exactly once"
    );
}

/// The operator has to do something, so the agent says so and stops rather than
/// printing the same 401 every 30 seconds forever.
#[tokio::test]
async fn a_dead_credential_ends_the_run() {
    let credential: CredentialProvider = Arc::new(|| {
        Box::pin(async {
            Err(horsie_runtime_vendor::CredentialError::Dead(
                "logged out".to_string(),
            ))
        })
    });
    let err = bare_vendor()
        .run(UNDIALABLE, credential, CancellationToken::new())
        .await
        .expect_err("a dead credential must end the run, not loop on it");
    assert!(
        matches!(&err, AgentExit::Fatal(e) if e.contains("logged out")),
        "{err:?}"
    );
}

/// The other half: an issuer that is merely unreachable must not take the agent
/// down — that would turn a server restart into a manual recovery.
#[tokio::test]
async fn a_transient_credential_failure_keeps_retrying() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let credential: CredentialProvider = Arc::new(move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            Err(horsie_runtime_vendor::CredentialError::Transient(
                "issuer unreachable".to_string(),
            ))
        })
    });

    let cancel = CancellationToken::new();
    let running = tokio::spawn({
        let cancel = cancel.clone();
        async move { bare_vendor().run(UNDIALABLE, credential, cancel).await }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !running.is_finished(),
        "a transient failure must not end the run"
    );
    cancel.cancel();
    running
        .await
        .expect("join")
        .expect("cancellation is not an error");

    assert!(calls.load(std::sync::atomic::Ordering::SeqCst) > 1);
}

// ── restart resilience ───────────────────────────────────────────────────────

/// The bug this whole change exists for. Ctrl-C on `horsie connect` used to
/// take every session on the machine with it: the runtimes lived only in the
/// agent's memory, so the next get answered `Gone` and the server wrote the
/// session off permanently.
#[tokio::test]
async fn a_respawnable_runtime_outlives_the_agent_process() {
    let machine = Machine::new();
    let first = machine.start(None, true).await;
    first
        .link
        .create("rt-1", &first.spec())
        .await
        .expect("create");
    drop(first);

    let second = machine.start(None, true).await;
    second
        .link
        .get("rt-1")
        .await
        .expect("a get after an agent restart must rebuild, not report it gone");
    assert!(second.is_live("rt-1").await);
}

/// The other half of that contract, and the reason it is opt-in: a vendor that
/// provisions its own workspace would re-clone on a respawn, so for it a
/// missing runtime stays terminal.
#[tokio::test]
async fn a_non_respawnable_agent_still_reports_it_gone_after_a_restart() {
    let machine = Machine::new();
    let first = machine.start(None, false).await;
    first
        .link
        .create("rt-1", &first.spec())
        .await
        .expect("create");
    drop(first);

    let second = machine.start(None, false).await;
    let err = second
        .link
        .get("rt-1")
        .await
        .expect_err("a provisioning vendor must not silently rebuild a workspace");
    assert!(matches!(err, RuntimeVendorError::Gone(_)), "{err:?}");
}

/// Hibernate is where the respawn path earns its keep in normal operation: the
/// process is freed while the session is idle, and the next turn brings it back.
#[tokio::test]
async fn hibernate_frees_the_process_and_a_get_brings_it_back() {
    let machine = Machine::new();
    let agent = machine.start(None, true).await;
    agent
        .link
        .create("rt-1", &agent.spec())
        .await
        .expect("create");
    assert!(agent.is_live("rt-1").await);

    agent.link.hibernate("rt-1").await;
    assert!(
        !agent.is_live("rt-1").await,
        "an agent that can rebuild the runtime should take the hint"
    );

    agent.link.get("rt-1").await.expect("a get resumes it");
    assert!(agent.is_live("rt-1").await);
}

/// A deleted session must not leave the means to rebuild its runtime lying
/// around: the directory is the record, so it goes with the session.
#[tokio::test]
async fn delete_removes_the_runtimes_state_directory() {
    let machine = Machine::new();
    let agent = machine.start(None, true).await;
    agent
        .link
        .create("rt-1", &agent.spec())
        .await
        .expect("create");
    let dir = machine.state_dir().join("rt-1");
    assert!(dir.join("spec.json").exists(), "create records the spec");

    agent.link.delete("rt-1").await;
    assert!(!dir.exists(), "delete takes the record with it");
}

/// The promise `lifecycle_locks` exists to keep. The real agent dispatches every
/// command on its own task, so without the per-id lock this `get` races ahead of
/// the create it belongs to and answers `Gone` for a runtime that is moments
/// from existing — which the session turns into a terminal, unrecoverable state.
#[tokio::test]
async fn get_during_an_in_flight_create_waits_for_it() {
    let (open, gate) = tokio::sync::watch::channel(false);
    let (_machine, agent) = start_agent(Some(gate)).await;

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
