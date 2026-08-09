//! A vendor process survives losing its link to the server.
//!
//! Everything here is real: a real `RuntimeVendorClient` dialing a real
//! `WebsocketRuntimeVendor` over a real TCP WebSocket, published through the real
//! `RuntimeVendorRegistry`. The only fixture is the runtime itself, because a
//! test that spawned `horsie-runtime` children would be measuring process
//! startup rather than reconnection.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use async_trait::async_trait;
use horsie_models::executor::RuntimeConfig;
use horsie_models::runtime::{BashInput, ToolCall};
use horsie_models::runtime_vendor::{
    QueryRuntimesRequest, RuntimeVendorCommand, RuntimeVendorEvent,
};
use horsie_runtime_client::{MockTransport, RuntimeClient};
use horsie_runtime_vendor::{
    AgentExit, Backoff, ConnectedRuntimeRegistry, FixedWorkspaces, HealthStatus, ProviderFactory,
    RuntimeError, RuntimeHandle, RuntimeProvider, RuntimeVendorClient, no_credential,
};
use horsie_server::auth::Principal;
use horsie_server::runtime_vendor::RuntimeVendor as _;
use horsie_server::runtime_vendor::fake::runtime_spec_fixture;
use horsie_server::runtime_vendor::{
    RuntimeVendorRegistry, RuntimeVendorTransport, WebsocketRuntimeVendor,
};
use horsie_server::sessions::spec::RuntimeVendorMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

// ── harness ──────────────────────────────────────────────────────────────────

/// A runtime that exists only in the agent's registry: creating it registers a
/// transport, which is all the agent's bookkeeping actually depends on.
struct StubProvider {
    connected: Arc<ConnectedRuntimeRegistry>,
}

#[async_trait]
impl RuntimeProvider for StubProvider {
    async fn create(
        &self,
        id: &str,
        _config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
        self.connected
            .register_transport(id.to_string(), Arc::new(MockTransport::ok("ok")))
            .await;
        Ok(Arc::new(StubHandle))
    }
}

struct StubHandle;

#[async_trait]
impl RuntimeHandle for StubHandle {
    async fn stop(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus, RuntimeError> {
        Ok(HealthStatus::Healthy)
    }
}

/// The server end: accept agents, hand each one to a real link, publish it.
async fn serve_vendor_connections(registry: Arc<RuntimeVendorRegistry>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let registry = registry.clone();
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(socket).await else {
                    return;
                };
                if let Ok(link) =
                    WebsocketRuntimeVendor::start(ws, Principal::Anonymous, registry.links()).await
                {
                    // Auth-disabled shape: one anonymous principal owns every
                    // name, so a reconnecting *process* still replaces its own
                    // entry — and, as of the name-collision gates, a second
                    // process does not.
                    match registry.publish(link.clone()) {
                        Ok(()) => {
                            let _ = link.confirm_registration().await;
                        }
                        Err(e) => {
                            link.reject_registration(&e.client_reason(link.vendor_name()))
                                .await;
                        }
                    }
                }
            });
        }
    });
    addr
}

/// A severable TCP hop between agent and server, standing in for the network.
///
/// Cutting it drops both sockets at once, which is what an agent sees when the
/// server restarts or the link blips — and unlike dropping the server-side
/// link, it is something a test can actually do, since the link's read loop
/// owns its socket on a task of its own.
struct Wire {
    addr: SocketAddr,
    hops: Arc<StdMutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Wire {
    async fn open(backend: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hops: Arc<StdMutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let accepted = hops.clone();
        tokio::spawn(async move {
            while let Ok((mut front, _)) = listener.accept().await {
                let Ok(mut back) = TcpStream::connect(backend).await else {
                    continue;
                };
                let hop = tokio::spawn(async move {
                    let _ = tokio::io::copy_bidirectional(&mut front, &mut back).await;
                });
                accepted.lock().unwrap().push(hop);
            }
        });
        Self { addr, hops }
    }

    fn cut(&self) {
        for hop in self.hops.lock().unwrap().drain(..) {
            hop.abort();
        }
    }
}

fn bash(command: &str) -> ToolCall {
    ToolCall::Bash(BashInput {
        command: command.to_string(),
        timeout_secs: None,
    })
}

/// Poll until `predicate` accepts the published vendor, or give up. Polling
/// rather than a notification because the agent reconnects on its own schedule
/// and nothing in the production path announces it to a test.
async fn await_vendor(
    links: &horsie_server::runtime_vendor::WebsocketVendorTable,
    name: &str,
    what: &str,
    predicate: impl Fn(&Arc<WebsocketRuntimeVendor>) -> bool,
) -> Arc<WebsocketRuntimeVendor> {
    for _ in 0..200 {
        let published = links.lock().unwrap().get(name).cloned();
        if let Some(link) = published
            && predicate(&link)
        {
            return link;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no vendor '{name}' that {what} after 5s");
}

/// A progress sink nothing reads: these tests assert on each operation's
/// return value, which is its outcome.
fn sink() -> horsie_runtime_vendor::RuntimeProgressSink {
    tokio::sync::mpsc::channel(8).0
}

// ── the test ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_vendor_agent_reconnects_after_its_link_drops_and_keeps_its_runtimes() {
    let vendors: RuntimeVendorMap = Arc::new(RwLock::new(HashMap::new()));
    let registry = Arc::new(RuntimeVendorRegistry::new(vendors.clone()));
    let links = registry.links();
    let server = serve_vendor_connections(registry.clone()).await;
    let wire = Wire::open(server).await;

    let tmp = tempfile::tempdir().unwrap();
    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let provider: ProviderFactory = {
        let connected = connected.clone();
        Arc::new(move |_id: &str, _caps: Option<PathBuf>| {
            Arc::new(StubProvider {
                connected: connected.clone(),
            })
        })
    };
    let workspaces = HashMap::from([("main".to_string(), tmp.path().to_path_buf())]);
    let agent = RuntimeVendorClient::new(
        "test-vendor".to_string(),
        false,
        provider,
        connected,
        Arc::new(FixedWorkspaces::new(workspaces)),
        tmp.path().join("state"),
    )
    // Milliseconds, so the test measures that reconnection happens rather than
    // how patient the production schedule is.
    .with_backoff(Backoff::new(
        Duration::from_millis(20),
        Duration::from_millis(100),
    ));

    let cancel = CancellationToken::new();
    let endpoint = format!("ws://{}/api/vendor/connect", wire.addr);
    let agent_cancel = cancel.clone();
    let agent_task =
        tokio::spawn(async move { agent.run(&endpoint, no_credential(), agent_cancel).await });

    let first = await_vendor(&links, "test-vendor", "registered", |_| true).await;
    first
        .create("rt-1", &runtime_spec_fixture("main").to_wire(), sink())
        .await
        .expect("the agent provisions a runtime over the first link");

    wire.cut();

    // Nobody restarts the process: the same agent comes back on a new link,
    // which `RuntimeVendorRegistry::register` swaps in under the same name.
    let second = await_vendor(&links, "test-vendor", "reconnected", |link| {
        !Arc::ptr_eq(link, &first) && link.is_reachable()
    })
    .await;
    assert!(
        !first.is_reachable(),
        "the link that was cut must be observably dead, not merely replaced"
    );

    // The runtime predates the disconnect. It is still the agent's, because a
    // dead socket to the server says nothing about the sandboxes running here.
    let listed = second
        .request(RuntimeVendorCommand::QueryRuntimes(QueryRuntimesRequest {}))
        .await
        .expect("the reconnected link answers");
    let RuntimeVendorEvent::QueryRuntimes(listed) = listed else {
        panic!("QueryRuntimes must be answered with a listing, got {listed:?}");
    };
    let ids: Vec<String> = listed.runtimes.into_iter().map(|r| r.runtime_id).collect();
    assert_eq!(ids, vec!["rt-1".to_string()]);

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), agent_task)
        .await
        .expect("cancellation must not have to wait out a backoff delay")
        .expect("the agent task must not panic")
        .expect("a cancelled agent exits cleanly");
}

/// The client an agent loop is executing against outlives a reconnect.
///
/// A run acquires its `RuntimeClient` once and bakes it into the toolbox for
/// the whole turn, so this is the client's real lifetime, not a contrived one.
/// Held against a link, every tool call after a blip failed `transport:
/// disconnected` and the model kept trying until it ran out of iterations.
#[tokio::test]
async fn a_held_runtime_client_keeps_working_across_a_reconnect() {
    let vendors: RuntimeVendorMap = Arc::new(RwLock::new(HashMap::new()));
    let registry = Arc::new(RuntimeVendorRegistry::new(vendors.clone()));
    let links = registry.links();
    let server = serve_vendor_connections(registry.clone()).await;
    let wire = Wire::open(server).await;

    let tmp = tempfile::tempdir().unwrap();
    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let provider: ProviderFactory = {
        let connected = connected.clone();
        Arc::new(move |_id: &str, _caps: Option<PathBuf>| {
            Arc::new(StubProvider {
                connected: connected.clone(),
            })
        })
    };
    let workspaces = HashMap::from([("main".to_string(), tmp.path().to_path_buf())]);
    let agent = RuntimeVendorClient::new(
        "test-vendor".to_string(),
        false,
        provider,
        connected,
        Arc::new(FixedWorkspaces::new(workspaces)),
        tmp.path().join("state"),
    )
    .with_backoff(Backoff::new(
        Duration::from_millis(20),
        Duration::from_millis(100),
    ));

    let cancel = CancellationToken::new();
    let endpoint = format!("ws://{}/api/vendor/connect", wire.addr);
    let agent_cancel = cancel.clone();
    let agent_task =
        tokio::spawn(async move { agent.run(&endpoint, no_credential(), agent_cancel).await });

    let first = await_vendor(&links, "test-vendor", "registered", |_| true).await;
    first
        .create("rt-1", &runtime_spec_fixture("main").to_wire(), sink())
        .await
        .expect("the agent provisions a runtime over the first link");

    // The client a run would hold: bound to the vendor's name, resolved through
    // the same registry the server keeps.
    let client = RuntimeClient::from_arc(
        Arc::new(RuntimeVendorTransport::new(
            links.clone(),
            "test-vendor".to_string(),
            "rt-1".to_string(),
        )),
        "rt-1",
    );
    client
        .invoke("tc1", bash("echo alive"))
        .await
        .expect("first call");

    wire.cut();
    await_vendor(&links, "test-vendor", "reconnected", |link| {
        !Arc::ptr_eq(link, &first) && link.is_reachable()
    })
    .await;

    let output = client
        .invoke("tc2", bash("echo alive"))
        .await
        .expect("the same client must reach the runtime over the new link");
    assert_eq!(output.stdout, "ok");

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), agent_task).await;
}

/// Two `horsie connect` processes, one name. The second must stop with a
/// reason instead of joining the flap war it used to start: displace the
/// incumbent, whose agent re-dials a second later and displaces it back, with
/// neither process reporting anything and sessions routed to whichever won the
/// last round.
#[tokio::test]
async fn a_second_agent_claiming_a_live_name_is_refused_and_stops() {
    let vendors: RuntimeVendorMap = Arc::new(RwLock::new(HashMap::new()));
    let registry = Arc::new(RuntimeVendorRegistry::new(vendors.clone()));
    let links = registry.links();
    let server = serve_vendor_connections(registry.clone()).await;

    let tmp = tempfile::tempdir().unwrap();
    let endpoint = format!("ws://{server}/api/vendor/connect");
    let root = tmp.path().to_path_buf();
    let build = move |dir: &str| -> RuntimeVendorClient {
        let root = root.clone();
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        let provider: ProviderFactory = {
            let connected = connected.clone();
            Arc::new(move |_id: &str, _caps: Option<PathBuf>| {
                Arc::new(StubProvider {
                    connected: connected.clone(),
                })
            })
        };
        let workspaces = HashMap::from([("main".to_string(), root.clone())]);
        RuntimeVendorClient::new(
            "horsie-local".to_string(),
            false,
            provider,
            connected,
            Arc::new(FixedWorkspaces::new(workspaces)),
            root.join(dir),
        )
        .with_backoff(Backoff::new(
            Duration::from_millis(20),
            Duration::from_millis(100),
        ))
    };

    // Two agent processes, built up front: separate state dirs, separate
    // instance ids, one name.
    let first = build("state-1");
    let second = build("state-2");

    let cancel = CancellationToken::new();
    let incumbent = tokio::spawn({
        let endpoint = endpoint.clone();
        let cancel = cancel.clone();
        async move { first.run(&endpoint, no_credential(), cancel).await }
    });
    let published = await_vendor(&links, "horsie-local", "registered", |link| {
        link.is_reachable()
    })
    .await;

    // A different process, same name, while the first is plainly alive.
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        second.run(&endpoint, no_credential(), CancellationToken::new()),
    )
    .await
    .expect("a refused agent must stop, not back off and retry")
    .expect_err("claiming a name in use is an error the operator has to see");
    assert!(
        matches!(&err, AgentExit::NameRefused(reason) if reason.contains("already in use")),
        "a refusal must be its own outcome, not a generic failure: {err:?}"
    );

    // The incumbent is untouched: same link, still connected, still serving.
    let held = links.lock().unwrap().get("horsie-local").cloned().unwrap();
    assert!(
        Arc::ptr_eq(&held, &published),
        "the refused agent must not have displaced the published link"
    );
    assert!(held.is_reachable());
    held.create("rt-1", &runtime_spec_fixture("main").to_wire(), sink())
        .await
        .expect("the incumbent still provisions runtimes");

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), incumbent).await;
}
