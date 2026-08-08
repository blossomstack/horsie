//! One connected vendor agent's WebSocket, with request/reply correlation.
//!
//! The agent owns runtime lifecycle; this link is the server's only handle on
//! it. Every command carries a fresh `request_id`; the read loop matches each
//! inbound event back to the waiter that issued it, and drops unsolicited
//! events (state changes, the boot `Ready`) rather than treating an
//! unmatched id as a protocol error.

use crate::auth::Principal;
use crate::runtime_vendor::RuntimeVendorError;
use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime_vendor::{
    CreateRuntimeRequest, DeleteRuntimeRequest, GetRuntimeRequest, HibernateRuntimeRequest,
    RuntimeSpec as WireRuntimeSpec, RuntimeVendorCommand, RuntimeVendorEvent,
    RuntimeVendorInboundMessage, RuntimeVendorOutboundMessage, VendorRegistered, VendorRejected,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// How long the agent has to announce itself after connecting.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a link may go without a single inbound frame before it is treated
/// as dead.
///
/// Agents ping every `HEARTBEAT_INTERVAL` (15s, agent-side), so three missed
/// pings end the link. It has to be this side of the deal: a half-open socket —
/// a slept laptop, a dropped VPN — never errors, so without a deadline the
/// entry would hold its vendor name until TCP eventually noticed, and the name
/// is exactly what a returning agent needs back.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Ceiling on a single command. A create with `git_checkout` provision steps
/// legitimately runs for minutes, so this matches the executor's existing
/// provision window rather than a typical request timeout.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

type Waiters = Arc<Mutex<HashMap<String, oneshot::Sender<RuntimeVendorEvent>>>>;

/// A sink that erases the socket type, so `WebsocketRuntimeVendor` is not generic over the
/// transport once constructed.
type BoxedSink = Box<
    dyn futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Send + Unpin,
>;

pub struct WebsocketRuntimeVendor {
    vendor_name: String,
    /// The announcing *process*. Two links carrying the same id are the same
    /// agent before and after a dropped socket; two carrying different ids are
    /// two agents, whatever else they have in common.
    instance_id: String,
    /// Who presented the credential this link was accepted on. A vendor name is
    /// owned by its principal, so a stranger cannot displace it.
    owner: Principal,
    capabilities: horsie_models::runtime_vendor::RuntimeVendorCapabilities,
    /// The `request_id` the agent put on its `Ready`. The registration verdict
    /// echoes it, so accepting or refusing an agent is an ordinary correlated
    /// reply rather than a second handshake grammar.
    ready_request_id: String,
    sink: Mutex<BoxedSink>,
    waiters: Waiters,
    connected: Arc<AtomicBool>,
    /// Fires once the read loop has ended, so whoever published this link can
    /// unpublish it. A watch rather than a notify: a waiter that arrives after
    /// the socket died must still be told, and `borrow()` after `subscribe()`
    /// closes that race.
    closed: Arc<tokio::sync::watch::Sender<bool>>,
    /// The `Arc` this link lives in, held weakly so the link never keeps
    /// itself alive.
    this: std::sync::Weak<WebsocketRuntimeVendor>,
    /// The account's vendor map, so a handle can resolve *the live link for
    /// this vendor name* on every call rather than capturing this one. A
    /// reconnect publishes a new link under the same name, and a handle that
    /// captured the old one failed every remaining tool call in the turn
    /// (#187).
    vendors: crate::runtime_vendor::WebsocketVendorTable,
}

impl WebsocketRuntimeVendor {
    /// Handshake on an accepted agent connection and start its read loop.
    ///
    /// The first message must be `RuntimeVendorEvent::Ready`; anything else (or
    /// silence past [`HANDSHAKE_TIMEOUT`]) drops the connection.
    pub async fn start<S>(
        ws: WebSocketStream<S>,
        owner: Principal,
        vendors: crate::runtime_vendor::WebsocketVendorTable,
    ) -> Result<Arc<Self>, String>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, mut stream) = ws.split();

        let announced = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<RuntimeVendorOutboundMessage>(&text) {
                            Ok(msg) => match msg.event {
                                RuntimeVendorEvent::Ready(ev) => return Some((msg.request_id, ev)),
                                RuntimeVendorEvent::RuntimeStateChanged(_)
                                | RuntimeVendorEvent::CreateRuntime(_)
                                | RuntimeVendorEvent::GetRuntime(_)
                                | RuntimeVendorEvent::HibernateRuntime(_)
                                | RuntimeVendorEvent::DeleteRuntime(_)
                                | RuntimeVendorEvent::QueryRuntimes(_)
                                | RuntimeVendorEvent::Runtime(_)
                                | RuntimeVendorEvent::RequestFailed(_) => return None,
                            },
                            Err(_) => return None,
                        }
                    }
                    Some(Ok(Message::Binary(_)))
                    | Some(Ok(Message::Ping(_)))
                    | Some(Ok(Message::Pong(_)))
                    | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                }
            }
        })
        .await;

        let (ready_request_id, announced) = match announced {
            Ok(Some(msg)) => msg,
            Ok(None) => return Err("agent did not announce itself".to_string()),
            Err(_) => return Err("timed out waiting for the agent handshake".to_string()),
        };

        let waiters: Waiters = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let closed = Arc::new(tokio::sync::watch::Sender::new(false));
        let link = Arc::new_cyclic(|this| Self {
            vendor_name: announced.vendor_name,
            instance_id: announced.instance_id,
            owner,
            capabilities: announced.capabilities,
            ready_request_id,
            sink: Mutex::new(Box::new(sink)),
            waiters: waiters.clone(),
            connected: connected.clone(),
            closed: closed.clone(),
            this: this.clone(),
            vendors,
        });

        tokio::spawn(async move {
            loop {
                // Every inbound frame counts, not just the heartbeat: a link
                // carrying tool calls is alive by definition, and a busy agent
                // must never lose its name to a ping that queued behind them.
                let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                    Ok(Some(next)) => next,
                    Ok(None) => break,
                    Err(_) => {
                        tracing::info!(
                            timeout_secs = IDLE_TIMEOUT.as_secs(),
                            "vendor link: no frame within the idle timeout, treating it as dead"
                        );
                        break;
                    }
                };
                let text = match next {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Binary(_))
                    | Ok(Message::Ping(_))
                    | Ok(Message::Pong(_))
                    | Ok(Message::Frame(_)) => continue,
                    Ok(Message::Close(_)) | Err(_) => break,
                };
                let Ok(msg) = serde_json::from_str::<RuntimeVendorOutboundMessage>(&text) else {
                    tracing::warn!("vendor link: undecodable frame, ignoring");
                    continue;
                };
                if let RuntimeVendorEvent::RuntimeStateChanged(ev) = &msg.event {
                    tracing::debug!(
                        runtime = %ev.runtime_id,
                        state = ?ev.state,
                        "vendor link: runtime state changed"
                    );
                }
                // An unmatched id is an unsolicited event, not an error.
                if let Some(tx) = waiters.lock().await.remove(&msg.request_id) {
                    let _ = tx.send(msg.event);
                }
            }
            connected.store(false, Ordering::Relaxed);
            // Drop every waiter so no caller blocks on a dead socket: each
            // pending `request` sees its sender vanish and reports Disconnected.
            waiters.lock().await.clear();
            let _ = closed.send(true);
        });

        Ok(link)
    }

    #[must_use]
    pub fn vendor_name(&self) -> &str {
        &self.vendor_name
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[must_use]
    pub fn owner(&self) -> &Principal {
        &self.owner
    }

    /// Resolves once the read loop has ended, immediately if it already has.
    pub async fn closed(&self) {
        let mut rx = self.closed.subscribe();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }

    /// Tell the agent it is published. Until this arrives an agent cannot tell
    /// a live registration from a refused one — the socket looks the same.
    pub async fn confirm_registration(&self) -> Result<(), String> {
        self.write(&RuntimeVendorInboundMessage {
            request_id: self.ready_request_id.clone(),
            command: RuntimeVendorCommand::VendorRegistered(VendorRegistered {}),
        })
        .await
    }

    /// Tell the agent it is not published, and why.
    ///
    /// Best-effort: the caller drops the link straight after, which closes the
    /// socket, and an agent that never reads this still ends up disconnected —
    /// it just goes back to guessing, which is the state this whole message
    /// exists to end.
    pub async fn reject_registration(&self, reason: &str) {
        if let Err(e) = self
            .write(&RuntimeVendorInboundMessage {
                request_id: self.ready_request_id.clone(),
                command: RuntimeVendorCommand::VendorRejected(VendorRejected {
                    reason: reason.to_string(),
                }),
            })
            .await
        {
            tracing::debug!(error = %e, "could not deliver a registration refusal");
        }
    }

    #[must_use]
    pub fn announced_capabilities(
        &self,
    ) -> horsie_models::runtime_vendor::RuntimeVendorCapabilities {
        self.capabilities.clone()
    }

    #[must_use]
    pub fn is_reachable(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn arc_self(&self) -> Option<Arc<Self>> {
        self.this.upgrade()
    }

    async fn write(&self, msg: &RuntimeVendorInboundMessage) -> Result<(), String> {
        if !self.is_reachable() {
            return Err("vendor agent disconnected".to_string());
        }
        let json = serde_json::to_string(msg).map_err(|e| format!("encode command: {e}"))?;
        self.sink
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| format!("send to vendor agent: {e}"))
    }

    /// Send a command and await the event carrying the same `request_id`.
    pub async fn request(
        &self,
        command: RuntimeVendorCommand,
    ) -> Result<RuntimeVendorEvent, String> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(request_id.clone(), tx);

        let msg = RuntimeVendorInboundMessage {
            request_id: request_id.clone(),
            command,
        };
        if let Err(e) = self.write(&msg).await {
            self.waiters.lock().await.remove(&request_id);
            return Err(e);
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(RuntimeVendorEvent::RequestFailed(ev))) => Err(ev.message),
            Ok(Ok(event)) => Ok(event),
            // The sender was dropped: the read loop exited, i.e. the socket died.
            Ok(Err(_)) => Err("vendor agent disconnected".to_string()),
            Err(_) => {
                self.waiters.lock().await.remove(&request_id);
                Err("timed out waiting for the vendor agent".to_string())
            }
        }
    }

    /// Send a command that has no reply (a relayed `CancelCall`).
    pub async fn send_oneway(&self, command: RuntimeVendorCommand) -> Result<(), String> {
        self.write(&RuntimeVendorInboundMessage {
            request_id: Uuid::new_v4().to_string(),
            command,
        })
        .await
    }
}

#[async_trait::async_trait]
impl crate::runtime_vendor::RuntimeVendor for WebsocketRuntimeVendor {
    fn name(&self) -> &str {
        &self.vendor_name
    }

    fn capabilities(&self) -> horsie_models::runtime_vendor::RuntimeVendorCapabilities {
        self.capabilities.clone()
    }

    /// A dead socket is not a dead vendor: the process behind it re-dials and
    /// publishes a fresh link under the same name, so a caller waits rather
    /// than failing a turn.
    fn is_reachable(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Never reports intermediate progress, and that is not a shortcut: a
    /// `horsie connect` process answers `CreateRuntime` only once its runtime
    /// is up and has dialled back to it, so by the time this returns there is
    /// nothing left to wait for.
    async fn create(
        &self,
        runtime_id: &str,
        spec: &WireRuntimeSpec,
        _progress: horsie_runtime_vendor::RuntimeProgressSink,
    ) -> Result<horsie_runtime_vendor::RuntimeProgress, RuntimeVendorError> {
        let Some(me) = self.arc_self() else {
            return Err(RuntimeVendorError::Unavailable(
                "the vendor link was dropped".to_string(),
            ));
        };
        me.request(RuntimeVendorCommand::CreateRuntime(CreateRuntimeRequest {
            runtime_id: runtime_id.to_string(),
            spec: spec.clone(),
        }))
        .await
        .map_err(RuntimeVendorError::Provision)?;
        Ok(horsie_runtime_vendor::RuntimeProgress::Ready(
            self.handle(runtime_id),
        ))
    }

    /// A failure here means the vendor has nothing under this id, which is
    /// terminal for the owning session — rebuilding would silently destroy a
    /// workspace the user believes still holds their work.
    async fn get(
        &self,
        runtime_id: &str,
        spec: &WireRuntimeSpec,
        _progress: horsie_runtime_vendor::RuntimeProgressSink,
    ) -> Result<horsie_runtime_vendor::RuntimeProgress, RuntimeVendorError> {
        let Some(me) = self.arc_self() else {
            return Err(RuntimeVendorError::Unavailable(
                "the vendor link was dropped".to_string(),
            ));
        };
        if !me.is_reachable() {
            return Err(RuntimeVendorError::Unavailable(
                "the runtime vendor is disconnected".to_string(),
            ));
        }
        // The spec is not on the wire yet; that lands with the schema change
        // that lets a vendor stop keeping its own copy on disk. Accepting it
        // here first means the trait is already the shape that needs.
        let _ = spec;
        me.request(RuntimeVendorCommand::GetRuntime(GetRuntimeRequest {
            runtime_id: runtime_id.to_string(),
        }))
        .await
        .map_err(RuntimeVendorError::Gone)?;
        Ok(horsie_runtime_vendor::RuntimeProgress::Ready(
            self.handle(runtime_id),
        ))
    }

    /// Advisory. A vendor that is not there simply misses the hint, which is
    /// why this reports `Stopped` rather than failing.
    async fn hibernate(
        &self,
        runtime_id: &str,
        _progress: horsie_runtime_vendor::RuntimeProgressSink,
    ) -> Result<horsie_runtime_vendor::RuntimeProgress, RuntimeVendorError> {
        let _ = self
            .request(RuntimeVendorCommand::HibernateRuntime(
                HibernateRuntimeRequest {
                    runtime_id: runtime_id.to_string(),
                },
            ))
            .await;
        Ok(horsie_runtime_vendor::RuntimeProgress::Stopped)
    }

    async fn delete(
        &self,
        runtime_id: &str,
        _progress: horsie_runtime_vendor::RuntimeProgressSink,
    ) -> Result<horsie_runtime_vendor::RuntimeProgress, RuntimeVendorError> {
        let _ = self
            .request(RuntimeVendorCommand::DeleteRuntime(DeleteRuntimeRequest {
                runtime_id: runtime_id.to_string(),
            }))
            .await;
        Ok(horsie_runtime_vendor::RuntimeProgress::Gone {
            reason: "the owning session was deleted".to_string(),
        })
    }
}

impl WebsocketRuntimeVendor {
    /// A handle bound to this vendor's *name*, not to this link.
    ///
    /// The transport re-resolves the name on every call, so a reconnect
    /// mid-turn is invisible to the run already in flight.
    fn handle(&self, runtime_id: &str) -> Arc<dyn crate::runtime_vendor::RuntimeHandle> {
        Arc::new(horsie_runtime_vendor::RuntimeHandleImpl::new(
            runtime_id.to_string(),
            Arc::new(crate::runtime_vendor::RuntimeVendorTransport::new(
                self.vendors.clone(),
                self.vendor_name.clone(),
                runtime_id.to_string(),
            )),
            self.closed.subscribe(),
        ))
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
    use crate::runtime_vendor::RuntimeSpec;
    use crate::runtime_vendor::RuntimeVendor as _;

    /// A vendor table of its own. Handles minted against it resolve back to
    /// whatever the test publishes.
    /// A progress sink nothing reads: these tests assert on the vendor's
    /// return value, which is the operation's outcome.
    fn sink() -> horsie_runtime_vendor::RuntimeProgressSink {
        tokio::sync::mpsc::channel(8).0
    }

    fn test_links() -> crate::runtime_vendor::WebsocketVendorTable {
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    use horsie_models::runtime_vendor::{
        QueryRuntimesResponse, RuntimeStateChanged, RuntimeVendorCapabilities, RuntimeVendorReady,
    };
    use tokio_tungstenite::tungstenite::protocol::Role;

    type AgentWs = WebSocketStream<tokio::io::DuplexStream>;

    /// A duplex-backed pair: (server-side link input, agent-side raw stream).
    /// A real WebSocket codec over an in-process pipe — no TCP, and crucially
    /// no in-memory transport type.
    async fn ws_pair() -> (AgentWs, AgentWs) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let agent = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        (server, agent)
    }

    async fn send_event(sink: &mut AgentWs, request_id: &str, event: RuntimeVendorEvent) {
        let msg = RuntimeVendorOutboundMessage {
            request_id: request_id.to_string(),
            event,
        };
        sink.send(Message::Text(serde_json::to_string(&msg).unwrap().into()))
            .await
            .unwrap();
    }

    fn boot(name: &str, provisioning: bool) -> RuntimeVendorEvent {
        RuntimeVendorEvent::Ready(RuntimeVendorReady {
            vendor_name: name.to_string(),
            instance_id: format!("{name}-instance"),
            capabilities: RuntimeVendorCapabilities {
                supports_provisioning: provisioning,
            },
        })
    }

    fn spec_fixture() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![crate::runtime_vendor::WorkspaceSpec {
                name: "main".to_string(),
            }],
            provision: vec![],
            env: vec![],
        }
    }

    #[tokio::test]
    async fn start_requires_a_registered_handshake_and_exposes_the_name() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("my-laptop", false)).await;
            std::future::pending::<()>().await;
        });

        let link = WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links())
            .await
            .expect("handshake");
        assert_eq!(link.vendor_name(), "my-laptop");
        assert_eq!(link.instance_id(), "my-laptop-instance");
        assert!(!link.announced_capabilities().supports_provisioning);
        assert!(link.is_reachable());
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_that_goes_quiet_is_treated_as_dead() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("slept-laptop", false)).await;
            // Holds the socket open and says nothing more — a half-open socket
            // reads exactly like this, and never errors on its own.
            std::future::pending::<()>().await;
        });

        let link = WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links())
            .await
            .expect("handshake");
        assert!(link.is_reachable());

        // Auto-advanced by the paused clock, not slept through.
        link.closed().await;
        assert!(
            !link.is_reachable(),
            "a link with no frame inside the idle timeout must not keep its name"
        );
    }

    #[tokio::test]
    async fn a_heartbeat_keeps_a_link_alive() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("busy-laptop", false)).await;
            loop {
                if agent_ws
                    .send(Message::Ping(Vec::new().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });

        let link = WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links())
            .await
            .expect("handshake");
        // Well short of the idle timeout, but the point is the pings are seen
        // as liveness rather than skipped as noise.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(link.is_reachable());
    }

    #[tokio::test]
    async fn start_rejects_an_agent_that_never_announces() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(
                &mut agent_ws,
                "wrong",
                RuntimeVendorEvent::QueryRuntimes(QueryRuntimesResponse { runtimes: vec![] }),
            )
            .await;
            std::future::pending::<()>().await;
        });
        let outcome =
            WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links()).await;
        let Err(err) = outcome else {
            panic!("a non-Ready first message must be rejected");
        };
        assert!(err.contains("announce"), "{err}");
    }

    #[tokio::test]
    async fn request_correlates_the_reply_by_request_id() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("v", true)).await;
            while let Some(Ok(Message::Text(text))) = agent_ws.next().await {
                let inbound: RuntimeVendorInboundMessage = serde_json::from_str(&text).unwrap();
                // An unsolicited event first, proving it is ignored rather than
                // mistaken for the reply.
                send_event(
                    &mut agent_ws,
                    "unsolicited",
                    RuntimeVendorEvent::RuntimeStateChanged(RuntimeStateChanged {
                        runtime_id: "rt-1".to_string(),
                        state: horsie_models::executor::RuntimeState::Running,
                    }),
                )
                .await;
                send_event(
                    &mut agent_ws,
                    &inbound.request_id,
                    RuntimeVendorEvent::QueryRuntimes(QueryRuntimesResponse { runtimes: vec![] }),
                )
                .await;
            }
        });

        let link = WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links())
            .await
            .expect("handshake");
        let event = link
            .request(RuntimeVendorCommand::QueryRuntimes(
                horsie_models::runtime_vendor::QueryRuntimesRequest {},
            ))
            .await
            .expect("reply");
        assert!(matches!(event, RuntimeVendorEvent::QueryRuntimes(_)));
    }

    #[tokio::test]
    async fn request_fails_when_the_agent_reports_command_failed() {
        use horsie_models::runtime_vendor::RequestFailed;
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("v", true)).await;
            while let Some(Ok(Message::Text(text))) = agent_ws.next().await {
                let inbound: RuntimeVendorInboundMessage = serde_json::from_str(&text).unwrap();
                send_event(
                    &mut agent_ws,
                    &inbound.request_id,
                    RuntimeVendorEvent::RequestFailed(RequestFailed {
                        message: "no such workspace 'ghost'".to_string(),
                    }),
                )
                .await;
            }
        });

        let link = WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links())
            .await
            .expect("handshake");
        let Err(err) = link.create("rt-1", &spec_fixture().to_wire(), sink()).await else {
            panic!("a RequestFailed reply must surface as an error");
        };
        assert!(format!("{err}").contains("ghost"), "{err}");
    }

    #[tokio::test]
    async fn dropped_link_marks_disconnected_and_fails_pending_requests() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("v", true)).await;
            let _ = agent_ws.next().await;
            drop(agent_ws);
        });

        let link = WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links())
            .await
            .expect("handshake");
        let err = link
            .request(RuntimeVendorCommand::QueryRuntimes(
                horsie_models::runtime_vendor::QueryRuntimesRequest {},
            ))
            .await
            .expect_err("a hung-up agent must fail the request, not hang");
        assert!(err.to_lowercase().contains("disconnect"), "{err}");
        for _ in 0..50 {
            if !link.is_reachable() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("link never observed the disconnect");
    }

    #[tokio::test]
    async fn create_hibernate_delete_emit_three_distinct_signals() {
        use std::sync::Mutex as StdMutex;
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let recorder = seen.clone();

        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("v", true)).await;
            while let Some(Ok(Message::Text(text))) = agent_ws.next().await {
                let inbound: RuntimeVendorInboundMessage = serde_json::from_str(&text).unwrap();
                let label = match &inbound.command {
                    RuntimeVendorCommand::CreateRuntime(_) => "create",
                    RuntimeVendorCommand::GetRuntime(_) => "get",
                    RuntimeVendorCommand::HibernateRuntime(_) => "hibernate",
                    RuntimeVendorCommand::DeleteRuntime(_) => "delete",
                    RuntimeVendorCommand::QueryRuntimes(_) => "query",
                    RuntimeVendorCommand::Runtime(_) => "runtime",
                    RuntimeVendorCommand::VendorRegistered(_) => "registered",
                    RuntimeVendorCommand::VendorRejected(_) => "rejected",
                };
                recorder.lock().unwrap().push(label.to_string());
                send_event(
                    &mut agent_ws,
                    &inbound.request_id,
                    RuntimeVendorEvent::RuntimeStateChanged(RuntimeStateChanged {
                        runtime_id: "rt-1".to_string(),
                        state: horsie_models::executor::RuntimeState::Running,
                    }),
                )
                .await;
            }
        });

        let link = WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links())
            .await
            .expect("handshake");
        link.create("rt-1", &spec_fixture().to_wire(), sink())
            .await
            .expect("create");
        let _ = link.hibernate("rt-1", sink()).await;
        let _ = link.delete("rt-1", sink()).await;

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[
                "create".to_string(),
                "hibernate".to_string(),
                "delete".to_string()
            ],
            "every lifecycle action must be its own explicit signal"
        );
    }

    #[tokio::test]
    async fn capabilities_come_from_the_agents_announcement() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("fixed-dir", false)).await;
            std::future::pending::<()>().await;
        });
        let link = WebsocketRuntimeVendor::start(server_ws, Principal::Anonymous, test_links())
            .await
            .expect("handshake");
        assert!(
            !link.capabilities().supports_provisioning,
            "the server must not second-guess what the agent announced"
        );
    }
}
