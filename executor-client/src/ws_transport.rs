use crate::client::ClientError;
use crate::transport::ExecutorTransport;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, ExecutorCommand, ExecutorEvent, ExecutorInboundMessage,
    ExecutorOutboundMessage, ToolCallCmd,
};
use models::runtime::{ToolCall, ToolCallRequest, ToolResult};
use runtime_client::{RuntimeTransport, TransportError};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use uuid::Uuid;

type Sink = Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>;
type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<ExecutorEvent>>>>;

/// Server-side lifecycle transport wrapping an accepted executor WS connection.
/// `runtime_transport` hands back a relay that shares this connection's sender +
/// pending map, so tool calls ride the same client↔executor socket.
pub struct WsExecutorTransport {
    sender: Sink,
    pending: Pending,
}

impl WsExecutorTransport {
    /// Wrap an accepted WebSocket stream. Consumes the `Registered` handshake event,
    /// then routes subsequent events to pending callers by `request_id`.
    pub fn accept(ws: WebSocketStream<TcpStream>) -> Self {
        let (sink, mut stream) = ws.split();
        let sender = Arc::new(Mutex::new(sink));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();

        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                if let Ok(msg) = serde_json::from_str::<ExecutorOutboundMessage>(&text) {
                    if matches!(msg.event, ExecutorEvent::Registered(_)) {
                        continue;
                    }
                    if let Some(tx) = pending_clone.lock().await.get(&msg.request_id) {
                        let _ = tx.send(msg.event).await;
                    }
                }
            }
        });

        Self { sender, pending }
    }
}

async fn send_command(
    sender: &Sink,
    pending: &Pending,
    request_id: &str,
    cmd: ExecutorCommand,
) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError> {
    let (tx, rx) = mpsc::channel(16);
    pending.lock().await.insert(request_id.to_string(), tx);
    let msg = ExecutorInboundMessage {
        request_id: request_id.to_string(),
        command: cmd,
    };
    let json =
        serde_json::to_string(&msg).map_err(|e| ClientError::Serialization(e.to_string()))?;
    sender
        .lock()
        .await
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| ClientError::SendFailed(e.to_string()))?;
    Ok(rx)
}

#[async_trait]
impl ExecutorTransport for WsExecutorTransport {
    async fn send(
        &self,
        request_id: &str,
        cmd: ExecutorCommand,
    ) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError> {
        send_command(&self.sender, &self.pending, request_id, cmd).await
    }

    async fn runtime_transport(
        &self,
        runtime_id: &str,
    ) -> Result<Arc<dyn RuntimeTransport>, ClientError> {
        Ok(Arc::new(RelayRuntimeTransport {
            sender: self.sender.clone(),
            pending: self.pending.clone(),
            runtime_id: runtime_id.to_string(),
        }))
    }
}

/// Tool-call transport that relays through the executor over the shared
/// client↔executor WS connection (server / distributed mode).
struct RelayRuntimeTransport {
    sender: Sink,
    pending: Pending,
    runtime_id: String,
}

#[async_trait]
impl RuntimeTransport for RelayRuntimeTransport {
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        let mut rx = send_command(
            &self.sender,
            &self.pending,
            call_id,
            ExecutorCommand::ToolCall(ToolCallCmd {
                runtime_id: self.runtime_id.clone(),
                call: ToolCallRequest {
                    call_id: call_id.to_string(),
                    call,
                },
            }),
        )
        .await
        .map_err(|e| TransportError::SendFailed(e.to_string()))?;

        loop {
            match rx.recv().await {
                Some(ExecutorEvent::ToolResult(ev)) if ev.call_id == call_id => {
                    self.pending.lock().await.remove(call_id);
                    return Ok(ev.result);
                }
                Some(_) => continue,
                None => {
                    self.pending.lock().await.remove(call_id);
                    return Err(TransportError::Disconnected);
                }
            }
        }
    }

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError> {
        let _ = send_command(
            &self.sender,
            &self.pending,
            &Uuid::new_v4().to_string(),
            ExecutorCommand::CancelToolCall(CancelToolCallCmd {
                runtime_id: self.runtime_id.clone(),
                call_id: call_id.to_string(),
            }),
        )
        .await
        .map_err(|e| TransportError::SendFailed(e.to_string()))?;
        Ok(())
    }
}
