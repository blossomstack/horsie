//! One connected vendor agent's WebSocket, with request/reply correlation.
//!
//! The agent owns runtime lifecycle; this link is the server's only handle on
//! it. Every command carries a fresh `request_id`; the read loop matches each
//! inbound event back to the waiter that issued it, and drops unsolicited
//! events (state changes, the boot `Ready`) rather than treating an
//! unmatched id as a protocol error.

use crate::runtime_vendor::{
    RuntimeSpec, RuntimeVendorTransport, VendorError, VendorRuntime, VendorRuntimeHandle,
};
use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime_vendor::{
    AttachRuntimeRequest, CreateRuntimeRequest, DeleteRuntimeRequest,
    RuntimeSpec as WireRuntimeSpec, RuntimeVendorCommand, RuntimeVendorEvent,
    RuntimeVendorInboundMessage, RuntimeVendorOutboundMessage, StopRuntimeRequest,
};
use horsie_runtime_client::RuntimeClient;
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

/// Ceiling on a single command. A create with `git_checkout` provision steps
/// legitimately runs for minutes, so this matches the executor's existing
/// provision window rather than a typical request timeout.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

type Waiters = Arc<Mutex<HashMap<String, oneshot::Sender<RuntimeVendorEvent>>>>;

/// A sink that erases the socket type, so `RuntimeVendorLink` is not generic over the
/// transport once constructed.
type BoxedSink = Box<
    dyn futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Send + Unpin,
>;

pub struct RuntimeVendorLink {
    vendor_name: String,
    capabilities: horsie_models::runtime_vendor::RuntimeVendorCapabilities,
    sink: Mutex<BoxedSink>,
    waiters: Waiters,
    connected: Arc<AtomicBool>,
    /// The `Arc` this link lives in, held weakly so the link never keeps
    /// itself alive. Needed because `create`/`attach` hand an owned `Arc` to
    /// the transport and lifecycle handle they build.
    this: std::sync::Weak<RuntimeVendorLink>,
}

impl RuntimeVendorLink {
    /// Handshake on an accepted agent connection and start its read loop.
    ///
    /// The first message must be `RuntimeVendorEvent::Ready`; anything else (or
    /// silence past [`HANDSHAKE_TIMEOUT`]) drops the connection.
    pub async fn start<S>(ws: WebSocketStream<S>) -> Result<Arc<Self>, String>
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
                                RuntimeVendorEvent::Ready(ev) => return Some(ev),
                                RuntimeVendorEvent::RuntimeStateChanged(_)
                                | RuntimeVendorEvent::CreateRuntime(_)
                                | RuntimeVendorEvent::AttachRuntime(_)
                                | RuntimeVendorEvent::StopRuntime(_)
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

        let announced = match announced {
            Ok(Some(ev)) => ev,
            Ok(None) => return Err("agent did not announce itself".to_string()),
            Err(_) => return Err("timed out waiting for the agent handshake".to_string()),
        };

        let waiters: Waiters = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let link = Arc::new_cyclic(|this| Self {
            vendor_name: announced.vendor_name,
            capabilities: announced.capabilities,
            sink: Mutex::new(Box::new(sink)),
            waiters: waiters.clone(),
            connected: connected.clone(),
            this: this.clone(),
        });

        tokio::spawn(async move {
            while let Some(next) = stream.next().await {
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
        });

        Ok(link)
    }

    #[must_use]
    pub fn vendor_name(&self) -> &str {
        &self.vendor_name
    }

    #[must_use]
    pub fn announced_capabilities(
        &self,
    ) -> horsie_models::runtime_vendor::RuntimeVendorCapabilities {
        self.capabilities.clone()
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn arc_self(&self) -> Option<Arc<Self>> {
        self.this.upgrade()
    }

    async fn write(&self, msg: &RuntimeVendorInboundMessage) -> Result<(), String> {
        if !self.is_connected() {
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

    /// Translate the server-side spec into the wire request. The capability
    /// file is read here and inlined: a server-local path means nothing to a
    /// remote agent.
    fn runtime_spec(spec: &RuntimeSpec) -> Result<WireRuntimeSpec, String> {
        let sandbox_capabilities =
            horsie_models::capabilities::CapabilitySpec::load(&spec.capabilities_file)?;
        Ok(WireRuntimeSpec {
            workspaces: spec.workspaces.iter().map(|w| w.name.clone()).collect(),
            env: spec.env.clone(),
            provision: spec.provision.clone(),
            sandbox_capabilities: Some(sandbox_capabilities),
        })
    }

    async fn provision(
        self: &Arc<Self>,
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
        let wire_spec = Self::runtime_spec(spec).map_err(wrap)?;
        let command = if attach {
            RuntimeVendorCommand::AttachRuntime(AttachRuntimeRequest {
                runtime_id: runtime_id.to_string(),
                spec: wire_spec,
            })
        } else {
            RuntimeVendorCommand::CreateRuntime(CreateRuntimeRequest {
                runtime_id: runtime_id.to_string(),
                spec: wire_spec,
            })
        };
        // `request` already turns RequestFailed into Err, so any other reply
        // means the agent accepted the command.
        self.request(command).await.map_err(wrap)?;

        let transport = RuntimeVendorTransport::new(self.clone(), runtime_id.to_string());
        Ok(VendorRuntime {
            // The runtime's own id doubles as its main agent's identity: the
            // server passes the session id as `runtime_id`, and that is also
            // what the agent journal is keyed by (`agent/<session-uuid>`). A
            // subagent sharing this runtime derives its own handle with
            // `RuntimeClient::with_agent_id`.
            runtime_client: RuntimeClient::from_arc(Arc::new(transport), runtime_id),
            handle: Arc::new(LinkRuntimeHandle {
                link: self.clone(),
                runtime_id: runtime_id.to_string(),
            }),
        })
    }
}

/// The vendor surface the session layer drives. These were a `RuntimeVendor`
/// trait while the server had several vendor implementations of its own; every
/// vendor is a connected agent now, so there is exactly one implementor and the
/// trait was pure indirection.
impl RuntimeVendorLink {
    /// What the agent announced it can do with a session workspace.
    #[must_use]
    pub fn capabilities(&self) -> crate::runtime_vendor::VendorCapabilities {
        crate::runtime_vendor::VendorCapabilities {
            supports_provisioning: self.capabilities.supports_provisioning,
        }
    }

    /// Provision a brand-new runtime.
    pub async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        let Some(me) = self.arc_self() else {
            return Err(VendorError::Provision(
                "vendor link was dropped".to_string(),
            ));
        };
        me.provision(runtime_id, spec, false).await
    }

    /// Revive a preserved runtime. Agents that cannot resume in place
    /// provision a fresh instance against the same spec.
    pub async fn attach(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        let Some(me) = self.arc_self() else {
            return Err(VendorError::Attach("vendor link was dropped".to_string()));
        };
        me.provision(runtime_id, spec, true).await
    }

    /// The owning session was deleted; the agent decides the runtime's fate.
    pub async fn delete(&self, runtime_id: &str) {
        let _ = self
            .request(RuntimeVendorCommand::DeleteRuntime(DeleteRuntimeRequest {
                runtime_id: runtime_id.to_string(),
            }))
            .await;
    }
}

/// Lifecycle handle for one runtime on one agent. `stop` is the explicit
/// stop-preserve signal; the agent decides what preservation means.
struct LinkRuntimeHandle {
    link: Arc<RuntimeVendorLink>,
    runtime_id: String,
}

#[async_trait::async_trait]
impl VendorRuntimeHandle for LinkRuntimeHandle {
    async fn stop(&self) {
        let _ = self
            .link
            .request(RuntimeVendorCommand::StopRuntime(StopRuntimeRequest {
                runtime_id: self.runtime_id.clone(),
            }))
            .await;
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
            capabilities: RuntimeVendorCapabilities {
                supports_provisioning: provisioning,
            },
        })
    }

    fn caps_file() -> std::path::PathBuf {
        use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
        let dir = std::env::temp_dir().join(format!("horsie-link-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("capabilities.json");
        let spec = CapabilitySpec {
            network: NetworkPolicy::Block(BlockNetwork {}),
            grants: vec![],
            unsafe_seatbelt_rules: None,
        };
        std::fs::write(&path, serde_json::to_vec(&spec).unwrap()).unwrap();
        path
    }

    fn spec_fixture() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![crate::runtime_vendor::WorkspaceSpec {
                name: "main".to_string(),
            }],
            provision: vec![],
            env: vec![],
            capabilities_file: caps_file(),
        }
    }

    #[tokio::test]
    async fn start_requires_a_registered_handshake_and_exposes_the_name() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("my-laptop", false)).await;
            std::future::pending::<()>().await;
        });

        let link = RuntimeVendorLink::start(server_ws)
            .await
            .expect("handshake");
        assert_eq!(link.vendor_name(), "my-laptop");
        assert!(!link.announced_capabilities().supports_provisioning);
        assert!(link.is_connected());
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
        let outcome = RuntimeVendorLink::start(server_ws).await;
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

        let link = RuntimeVendorLink::start(server_ws)
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

        let link = RuntimeVendorLink::start(server_ws)
            .await
            .expect("handshake");
        let Err(err) = link.create("rt-1", &spec_fixture()).await else {
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

        let link = RuntimeVendorLink::start(server_ws)
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
            if !link.is_connected() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("link never observed the disconnect");
    }

    #[tokio::test]
    async fn create_stop_delete_emit_three_distinct_signals() {
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
                    RuntimeVendorCommand::AttachRuntime(_) => "attach",
                    RuntimeVendorCommand::StopRuntime(_) => "stop",
                    RuntimeVendorCommand::DeleteRuntime(_) => "delete",
                    RuntimeVendorCommand::QueryRuntimes(_) => "query",
                    RuntimeVendorCommand::Runtime(_) => "runtime",
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

        let link = RuntimeVendorLink::start(server_ws)
            .await
            .expect("handshake");
        let rt = link.create("rt-1", &spec_fixture()).await.expect("create");
        rt.handle.stop().await;
        link.delete("rt-1").await;

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[
                "create".to_string(),
                "stop".to_string(),
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
        let link = RuntimeVendorLink::start(server_ws)
            .await
            .expect("handshake");
        assert!(
            !link.capabilities().supports_provisioning,
            "the server must not second-guess what the agent announced"
        );
    }
}
