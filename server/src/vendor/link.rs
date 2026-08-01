//! One connected vendor agent's WebSocket, with request/reply correlation.
//!
//! The agent owns runtime lifecycle; this link is the server's only handle on
//! it. Every command carries a fresh `request_id`; the read loop matches each
//! inbound event back to the waiter that issued it, and drops unsolicited
//! events (state changes, the boot `Registered`) rather than treating an
//! unmatched id as a protocol error.

use crate::vendor::{
    RuntimeSpec, RuntimeVendor, VendorError, VendorRuntime, VendorRuntimeHandle,
    VendorRuntimeTransport,
};
use futures_util::{SinkExt, StreamExt};
use horsie_models::vendor::{
    VendorAttachRuntime, VendorCommand, VendorCreateRuntime, VendorDeleteRuntime, VendorEvent,
    VendorInboundMessage, VendorOutboundMessage, VendorRuntimeRequest, VendorStopRuntime,
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

type Waiters = Arc<Mutex<HashMap<String, oneshot::Sender<VendorEvent>>>>;

/// A sink that erases the socket type, so `VendorLink` is not generic over the
/// transport once constructed.
type BoxedSink = Box<
    dyn futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Send + Unpin,
>;

pub struct VendorLink {
    vendor_name: String,
    capabilities: horsie_models::vendor::VendorAgentCapabilities,
    sink: Mutex<BoxedSink>,
    waiters: Waiters,
    connected: Arc<AtomicBool>,
    /// The `Arc` this link lives in, held weakly so the link never keeps
    /// itself alive. Needed because `create`/`attach` hand an owned `Arc` to
    /// the transport and lifecycle handle they build.
    this: std::sync::Weak<VendorLink>,
}

impl VendorLink {
    /// Handshake on an accepted agent connection and start its read loop.
    ///
    /// The first message must be `VendorEvent::Registered`; anything else (or
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
                        match serde_json::from_str::<VendorOutboundMessage>(&text) {
                            Ok(msg) => match msg.event {
                                VendorEvent::Registered(ev) => return Some(ev),
                                VendorEvent::RuntimeStateChanged(_)
                                | VendorEvent::RuntimesListed(_)
                                | VendorEvent::CommandFailed(_)
                                | VendorEvent::ToolResult(_)
                                | VendorEvent::ScanResult(_)
                                | VendorEvent::SessionStartResult(_) => return None,
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
                let Ok(msg) = serde_json::from_str::<VendorOutboundMessage>(&text) else {
                    tracing::warn!("vendor link: undecodable frame, ignoring");
                    continue;
                };
                if let VendorEvent::RuntimeStateChanged(ev) = &msg.event {
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
    pub fn announced_capabilities(&self) -> horsie_models::vendor::VendorAgentCapabilities {
        self.capabilities.clone()
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn arc_self(&self) -> Option<Arc<Self>> {
        self.this.upgrade()
    }

    async fn write(&self, msg: &VendorInboundMessage) -> Result<(), String> {
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
    pub async fn request(&self, command: VendorCommand) -> Result<VendorEvent, String> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(request_id.clone(), tx);

        let msg = VendorInboundMessage {
            request_id: request_id.clone(),
            command,
        };
        if let Err(e) = self.write(&msg).await {
            self.waiters.lock().await.remove(&request_id);
            return Err(e);
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(VendorEvent::CommandFailed(ev))) => Err(ev.message),
            Ok(Ok(event)) => Ok(event),
            // The sender was dropped: the read loop exited, i.e. the socket died.
            Ok(Err(_)) => Err("vendor agent disconnected".to_string()),
            Err(_) => {
                self.waiters.lock().await.remove(&request_id);
                Err("timed out waiting for the vendor agent".to_string())
            }
        }
    }

    /// Send a command that has no reply (`CancelToolCall`).
    pub async fn send_oneway(&self, command: VendorCommand) -> Result<(), String> {
        self.write(&VendorInboundMessage {
            request_id: Uuid::new_v4().to_string(),
            command,
        })
        .await
    }

    /// Translate the server-side spec into the wire request. The capability
    /// file is read here and inlined: a server-local path means nothing to a
    /// remote agent.
    fn runtime_request(spec: &RuntimeSpec) -> Result<VendorRuntimeRequest, String> {
        let sandbox_capabilities =
            horsie_models::capabilities::CapabilitySpec::load(&spec.capabilities_file)?;
        Ok(VendorRuntimeRequest {
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
        let request = Self::runtime_request(spec).map_err(wrap)?;
        let command = if attach {
            VendorCommand::AttachRuntime(VendorAttachRuntime {
                runtime_id: runtime_id.to_string(),
                request,
            })
        } else {
            VendorCommand::CreateRuntime(VendorCreateRuntime {
                runtime_id: runtime_id.to_string(),
                request,
            })
        };
        // `request` already turns CommandFailed into Err, so any other reply
        // means the agent accepted the command.
        self.request(command).await.map_err(wrap)?;

        let transport = VendorRuntimeTransport::new(self.clone(), runtime_id.to_string());
        Ok(VendorRuntime {
            runtime_client: RuntimeClient::from_arc(Arc::new(transport)),
            handle: Arc::new(LinkRuntimeHandle {
                link: self.clone(),
                runtime_id: runtime_id.to_string(),
            }),
        })
    }
}

#[async_trait::async_trait]
impl RuntimeVendor for VendorLink {
    fn capabilities(&self) -> crate::vendor::VendorCapabilities {
        crate::vendor::VendorCapabilities {
            supports_provisioning: self.capabilities.supports_provisioning,
        }
    }

    async fn create(
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

    async fn attach(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        let Some(me) = self.arc_self() else {
            return Err(VendorError::Attach("vendor link was dropped".to_string()));
        };
        me.provision(runtime_id, spec, true).await
    }

    async fn delete(&self, runtime_id: &str) {
        let _ = self
            .request(VendorCommand::DeleteRuntime(VendorDeleteRuntime {
                runtime_id: runtime_id.to_string(),
            }))
            .await;
    }
}

/// Lifecycle handle for one runtime on one agent. `stop` is the explicit
/// stop-preserve signal; the agent decides what preservation means.
struct LinkRuntimeHandle {
    link: Arc<VendorLink>,
    runtime_id: String,
}

#[async_trait::async_trait]
impl VendorRuntimeHandle for LinkRuntimeHandle {
    async fn stop(&self) {
        let _ = self
            .link
            .request(VendorCommand::StopRuntime(VendorStopRuntime {
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
    use horsie_models::vendor::{
        VendorAgentCapabilities, VendorRegistered, VendorRuntimeStateChanged, VendorRuntimesListed,
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

    async fn send_event(sink: &mut AgentWs, request_id: &str, event: VendorEvent) {
        let msg = VendorOutboundMessage {
            request_id: request_id.to_string(),
            event,
        };
        sink.send(Message::Text(serde_json::to_string(&msg).unwrap().into()))
            .await
            .unwrap();
    }

    fn boot(name: &str, provisioning: bool) -> VendorEvent {
        VendorEvent::Registered(VendorRegistered {
            vendor_name: name.to_string(),
            capabilities: VendorAgentCapabilities {
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
            workspaces: vec![crate::vendor::WorkspaceSpec {
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

        let link = VendorLink::start(server_ws).await.expect("handshake");
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
                VendorEvent::RuntimesListed(VendorRuntimesListed { runtimes: vec![] }),
            )
            .await;
            std::future::pending::<()>().await;
        });
        let outcome = VendorLink::start(server_ws).await;
        let Err(err) = outcome else {
            panic!("a non-Registered first message must be rejected");
        };
        assert!(err.contains("announce"), "{err}");
    }

    #[tokio::test]
    async fn request_correlates_the_reply_by_request_id() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("v", true)).await;
            while let Some(Ok(Message::Text(text))) = agent_ws.next().await {
                let inbound: VendorInboundMessage = serde_json::from_str(&text).unwrap();
                // An unsolicited event first, proving it is ignored rather than
                // mistaken for the reply.
                send_event(
                    &mut agent_ws,
                    "unsolicited",
                    VendorEvent::RuntimeStateChanged(VendorRuntimeStateChanged {
                        runtime_id: "rt-1".to_string(),
                        state: horsie_models::executor::RuntimeState::Running,
                    }),
                )
                .await;
                send_event(
                    &mut agent_ws,
                    &inbound.request_id,
                    VendorEvent::RuntimesListed(VendorRuntimesListed { runtimes: vec![] }),
                )
                .await;
            }
        });

        let link = VendorLink::start(server_ws).await.expect("handshake");
        let event = link
            .request(VendorCommand::QueryRuntimes(
                horsie_models::vendor::VendorQueryRuntimes {},
            ))
            .await
            .expect("reply");
        assert!(matches!(event, VendorEvent::RuntimesListed(_)));
    }

    #[tokio::test]
    async fn request_fails_when_the_agent_reports_command_failed() {
        use horsie_models::vendor::VendorCommandFailed;
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(&mut agent_ws, "boot", boot("v", true)).await;
            while let Some(Ok(Message::Text(text))) = agent_ws.next().await {
                let inbound: VendorInboundMessage = serde_json::from_str(&text).unwrap();
                send_event(
                    &mut agent_ws,
                    &inbound.request_id,
                    VendorEvent::CommandFailed(VendorCommandFailed {
                        message: "no such workspace 'ghost'".to_string(),
                    }),
                )
                .await;
            }
        });

        let link = VendorLink::start(server_ws).await.expect("handshake");
        let Err(err) = link.create("rt-1", &spec_fixture()).await else {
            panic!("a CommandFailed reply must surface as an error");
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

        let link = VendorLink::start(server_ws).await.expect("handshake");
        let err = link
            .request(VendorCommand::QueryRuntimes(
                horsie_models::vendor::VendorQueryRuntimes {},
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
                let inbound: VendorInboundMessage = serde_json::from_str(&text).unwrap();
                let label = match &inbound.command {
                    VendorCommand::CreateRuntime(_) => "create",
                    VendorCommand::AttachRuntime(_) => "attach",
                    VendorCommand::StopRuntime(_) => "stop",
                    VendorCommand::DeleteRuntime(_) => "delete",
                    VendorCommand::QueryRuntimes(_) => "query",
                    VendorCommand::ToolCall(_) => "tool",
                    VendorCommand::CancelToolCall(_) => "cancel",
                    VendorCommand::ScanWorkspace(_) => "scan",
                    VendorCommand::SessionStart(_) => "session-start",
                };
                recorder.lock().unwrap().push(label.to_string());
                send_event(
                    &mut agent_ws,
                    &inbound.request_id,
                    VendorEvent::RuntimeStateChanged(VendorRuntimeStateChanged {
                        runtime_id: "rt-1".to_string(),
                        state: horsie_models::executor::RuntimeState::Running,
                    }),
                )
                .await;
            }
        });

        let link = VendorLink::start(server_ws).await.expect("handshake");
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
        let link = VendorLink::start(server_ws).await.expect("handshake");
        assert!(
            !RuntimeVendor::capabilities(link.as_ref()).supports_provisioning,
            "the server must not second-guess what the agent announced"
        );
    }
}
