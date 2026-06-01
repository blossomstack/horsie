use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use models::runtime::{
    CancelCallRequest, RuntimeInboundMessage, RuntimeOutboundMessage, ScanRequest, ToolCall,
    ToolCallRequest, ToolResult, WorkspaceScan,
};
use runtime_client::{RuntimeTransport, TransportError};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Reply = Result<ToolResult, TransportError>;
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Reply>>>>;
type ScanReply = Result<WorkspaceScan, TransportError>;
type PendingScan = Arc<Mutex<HashMap<String, oneshot::Sender<ScanReply>>>>;

/// Direct tool-call transport over a single accepted runtime link
/// (`WebSocketStream<S>`, where `S` = `TcpStream` or `UnixStream`). Owns the sink
/// and `call_id → oneshot` pending maps (one for tool calls, one for scans); a
/// spawned reader fills them and, on disconnect, resolves every outstanding call
/// with [`TransportError::Disconnected`].
pub struct SocketRuntimeTransport<S> {
    sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
    pending: Pending,
    pending_scan: PendingScan,
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
        let pending_scan: PendingScan = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let reader_pending_scan = pending_scan.clone();
        let (closed_tx, closed_rx) = oneshot::channel();
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                    Ok(RuntimeOutboundMessage::ToolCallResponse(resp)) => {
                        if let Some(tx) = reader_pending.lock().await.remove(&resp.call_id) {
                            let _ = tx.send(Ok(resp.result));
                        }
                    }
                    Ok(RuntimeOutboundMessage::ScanResult(resp)) => {
                        if let Some(tx) = reader_pending_scan.lock().await.remove(&resp.call_id) {
                            let _ = tx.send(Ok(resp.scan));
                        }
                    }
                    Ok(RuntimeOutboundMessage::Ready(_)) | Err(_) => {}
                }
            }
            // Disconnected: fail every outstanding call so no invoke()/scan() hangs
            // forever, then signal the link is closed.
            let mut map = reader_pending.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(TransportError::Disconnected));
            }
            drop(map);
            let mut scan_map = reader_pending_scan.lock().await;
            for (_, tx) in scan_map.drain() {
                let _ = tx.send(Err(TransportError::Disconnected));
            }
            drop(scan_map);
            let _ = closed_tx.send(());
        });
        (
            Self {
                sink: Arc::new(Mutex::new(sink)),
                pending,
                pending_scan,
            },
            closed_rx,
        )
    }
}

#[async_trait]
impl<S> RuntimeTransport for SocketRuntimeTransport<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(call_id.to_string(), tx);

        let msg = RuntimeInboundMessage::ToolCall(ToolCallRequest {
            call_id: call_id.to_string(),
            call,
        });
        let json = serde_json::to_string(&msg)
            .map_err(|e| TransportError::Serialization(e.to_string()))?;
        // The sink guard is released at the end of this statement, before we await
        // the response — so the reader task is never blocked behind it.
        if let Err(e) = self
            .sink
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
        {
            self.pending.lock().await.remove(call_id);
            return Err(TransportError::SendFailed(e.to_string()));
        }
        match rx.await {
            Ok(reply) => reply,
            Err(_) => Err(TransportError::Disconnected),
        }
    }

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError> {
        let msg = RuntimeInboundMessage::CancelCall(CancelCallRequest {
            call_id: call_id.to_string(),
        });
        let json = serde_json::to_string(&msg)
            .map_err(|e| TransportError::Serialization(e.to_string()))?;
        self.sink
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }

    async fn scan_workspace(
        &self,
        call_id: &str,
        instruction_candidates: Vec<String>,
        skills_glob: String,
    ) -> Result<WorkspaceScan, TransportError> {
        let (tx, rx) = oneshot::channel();
        self.pending_scan
            .lock()
            .await
            .insert(call_id.to_string(), tx);

        let msg = RuntimeInboundMessage::ScanWorkspace(ScanRequest {
            call_id: call_id.to_string(),
            instruction_candidates,
            skills_glob,
        });
        let json = serde_json::to_string(&msg)
            .map_err(|e| TransportError::Serialization(e.to_string()))?;
        if let Err(e) = self
            .sink
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
        {
            self.pending_scan.lock().await.remove(call_id);
            return Err(TransportError::SendFailed(e.to_string()));
        }
        match rx.await {
            Ok(reply) => reply,
            Err(_) => Err(TransportError::Disconnected),
        }
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
    use models::runtime::{BashInput, ToolCallResponse, ToolOutput};
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
                        });
                        let _ = sink
                            .send(Message::Text(serde_json::to_string(&resp).unwrap().into()))
                            .await;
                    }
                    Ok(RuntimeInboundMessage::ScanWorkspace(req)) => {
                        let resp =
                            RuntimeOutboundMessage::ScanResult(models::runtime::ScanResponse {
                                call_id: req.call_id,
                                scan: models::runtime::WorkspaceScan {
                                    instructions: Some(models::runtime::ScannedFile {
                                        path: "AGENTS.md".into(),
                                        content: "ctx".into(),
                                    }),
                                    skills: vec![],
                                },
                            });
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
        })
    }

    #[tokio::test]
    async fn invoke_correlates_response() {
        let (t, _dir) = paired().await;
        let r = t.invoke("c1", bash()).await.unwrap();
        assert!(matches!(r, ToolResult::Ok(o) if o.stdout == "ok"));
    }

    #[tokio::test]
    async fn scan_correlates_response() {
        let (t, _dir) = paired().await;
        let scan = t
            .scan_workspace(
                "s1",
                vec!["AGENTS.md".into()],
                ".claude/skills/*/SKILL.md".into(),
            )
            .await
            .unwrap();
        assert_eq!(scan.instructions.unwrap().content, "ctx");
    }

    #[tokio::test]
    async fn concurrent_invokes_each_resolve() {
        let (t, _dir) = paired().await;
        let t = Arc::new(t);
        let mut handles = Vec::new();
        for i in 0..8 {
            let t = t.clone();
            handles.push(tokio::spawn(async move {
                t.invoke(&format!("c{i}"), bash()).await
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
        let err = t.invoke("c1", bash()).await.unwrap_err();
        assert!(matches!(err, TransportError::Disconnected));
    }
}
