use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime::{RuntimeInboundMessage, RuntimeOutboundMessage};
use horsie_runtime_client::{RuntimeTransport, TransportError, inbound_call_id, outbound_call_id};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Reply = Result<RuntimeOutboundMessage, TransportError>;
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Reply>>>>;

/// Direct runtime transport over a single accepted link (`WebSocketStream<S>`,
/// where `S` = `TcpStream` or `UnixStream`). Owns the sink and one
/// `call_id → oneshot` pending map; a spawned reader fills it and, on disconnect,
/// resolves every outstanding request with [`TransportError::Disconnected`].
///
/// The map is keyed by correlation id alone, not by request kind — which is what
/// lets a new runtime message ride this transport without touching it.
pub struct SocketRuntimeTransport<S> {
    sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
    pending: Pending,
}

/// The unix instantiation used by CLI mode.
pub type UnixSocketRuntimeTransport = SocketRuntimeTransport<tokio::net::UnixStream>;

impl<S> SocketRuntimeTransport<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(ws: WebSocketStream<S>) -> Self {
        let (sink, stream) = ws.split();
        Self::from_split(sink, stream).0
    }

    /// Build the transport over already-split halves. Returns the transport and a
    /// `closed` receiver that resolves when the runtime link drops, so the owner
    /// (e.g. the connection handler) can deregister it.
    pub fn from_split(
        sink: SplitSink<WebSocketStream<S>, Message>,
        mut stream: SplitStream<WebSocketStream<S>>,
    ) -> (Self, oneshot::Receiver<()>) {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let (closed_tx, closed_rx) = oneshot::channel();
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                let Ok(msg) = serde_json::from_str::<RuntimeOutboundMessage>(&text) else {
                    continue;
                };
                // A reply with no correlation id is a handshake message (Ready /
                // provisioning lifecycle), resolved before the transport takes
                // over the link; an unmatched id is a reply to a request that
                // already gave up.
                let Some(call_id) = outbound_call_id(&msg) else {
                    continue;
                };
                let waiter = reader_pending.lock().await.remove(call_id);
                if let Some(tx) = waiter {
                    let _ = tx.send(Ok(msg));
                }
            }
            // Disconnected: fail every outstanding request so nothing hangs
            // forever, then signal the link is closed.
            let mut map = reader_pending.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(TransportError::Disconnected));
            }
            drop(map);
            let _ = closed_tx.send(());
        });
        (
            Self {
                sink: Arc::new(Mutex::new(sink)),
                pending,
            },
            closed_rx,
        )
    }

    async fn write(&self, message: &RuntimeInboundMessage) -> Result<(), TransportError> {
        let json = serde_json::to_string(message)
            .map_err(|e| TransportError::Serialization(e.to_string()))?;
        self.sink
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }
}

#[async_trait]
impl<S> RuntimeTransport for SocketRuntimeTransport<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn relay(
        &self,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError> {
        let call_id = inbound_call_id(&message).to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(call_id.clone(), tx);

        // The sink guard is released inside `write`, before we await the reply —
        // so the reader task is never blocked behind it.
        if let Err(e) = self.write(&message).await {
            self.pending.lock().await.remove(&call_id);
            return Err(e);
        }
        match rx.await {
            Ok(reply) => reply,
            Err(_) => Err(TransportError::Disconnected),
        }
    }

    async fn send_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError> {
        self.write(&message).await
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
    use horsie_models::runtime::{BashInput, ToolCall, ToolCallResponse, ToolOutput, ToolResult};
    use tokio::net::{UnixListener, UnixStream};

    /// A fake runtime on the server side of a paired unix socket that answers every
    /// ToolCall with `stdout = "ok"`.
    async fn paired() -> (UnixSocketRuntimeTransport, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("rt.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut stream) = ws.split();
            while let Some(Ok(Message::Text(t))) = stream.next().await {
                match serde_json::from_str::<RuntimeInboundMessage>(&t) {
                    Ok(RuntimeInboundMessage::ToolCall(req)) => {
                        let resp = RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                            call_id: req.call_id,
                            result: ToolResult::Ok(ToolOutput {
                                stdout: "ok".into(),
                                stderr: String::new(),
                                exit_code: 0,
                            }),
                            hooks: Vec::new(),
                        });
                        let _ = sink
                            .send(Message::Text(serde_json::to_string(&resp).unwrap().into()))
                            .await;
                    }
                    Ok(RuntimeInboundMessage::ScanWorkspace(req)) => {
                        let resp = RuntimeOutboundMessage::ScanResult(
                            horsie_models::runtime::ScanResponse {
                                shared_skills: vec![],
                                shared_agents: None,
                                shared_commands: None,
                                shared_root: None,
                                call_id: req.call_id,
                                workspaces: vec![horsie_models::runtime::WorkspaceScan {
                                    name: "october".into(),
                                    path: "/ws/october".into(),
                                    is_git_repo: false,
                                    instructions: Some(horsie_models::runtime::ScannedFile {
                                        path: "AGENTS.md".into(),
                                        content: "ctx".into(),
                                    }),
                                    skills: vec![],
                                    platform: None,
                                }],
                            },
                        );
                        let _ = sink
                            .send(Message::Text(serde_json::to_string(&resp).unwrap().into()))
                            .await;
                    }
                    _ => {}
                }
            }
        });
        let client = UnixStream::connect(&path).await.unwrap();
        let ws = tokio_tungstenite::client_async("ws://localhost/", client)
            .await
            .unwrap()
            .0;
        (SocketRuntimeTransport::new(ws), dir)
    }

    fn bash() -> ToolCall {
        ToolCall::Bash(BashInput {
            command: "x".into(),
            timeout_secs: None,
        })
    }

    #[tokio::test]
    async fn invoke_correlates_response() {
        let (t, _dir) = paired().await;
        let r = t.invoke("c1", "agent-1", bash()).await.unwrap();
        assert!(matches!(r, (ToolResult::Ok(o), _) if o.stdout == "ok"));
    }

    #[tokio::test]
    async fn scan_correlates_response() {
        let (t, _dir) = paired().await;
        let resp = t
            .scan_workspace(
                "s1",
                None,
                vec!["AGENTS.md".into()],
                ".claude/skills/*/SKILL.md".into(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(resp.workspaces.len(), 1);
        assert_eq!(
            resp.workspaces[0].instructions.as_ref().unwrap().content,
            "ctx"
        );
    }

    #[tokio::test]
    async fn concurrent_invokes_each_resolve() {
        let (t, _dir) = paired().await;
        let t = Arc::new(t);
        let mut handles = Vec::new();
        for i in 0..8 {
            let t = t.clone();
            handles.push(tokio::spawn(async move {
                t.invoke(&format!("c{i}"), "agent-1", bash()).await
            }));
        }
        for h in handles {
            assert!(h.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn disconnect_resolves_pending_with_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("rt.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // Server accepts, reads one frame, then drops the connection without replying.
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (_sink, mut stream) = ws.split();
            let _ = stream.next().await;
        });
        let client = UnixStream::connect(&path).await.unwrap();
        let ws = tokio_tungstenite::client_async("ws://localhost/", client)
            .await
            .unwrap()
            .0;
        let t = SocketRuntimeTransport::new(ws);
        let err = t.invoke("c1", "agent-1", bash()).await.unwrap_err();
        assert!(matches!(err, TransportError::Disconnected));
    }
}
