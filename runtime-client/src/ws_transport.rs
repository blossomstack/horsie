use crate::transport::{RuntimeTransport, TransportError};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, ExecutorCommand, ExecutorEvent, ExecutorInboundMessage,
    ExecutorOutboundMessage, ToolCallCmd,
};
use models::runtime::{ToolCall, ToolCallRequest, ToolResult};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use uuid::Uuid;

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<ExecutorEvent>>>>;

/// Server-side WS transport. Wraps the connection the executor dials into.
/// Translates RuntimeTransport::invoke into ToolCallCmd + ToolResultEvent correlation.
pub struct ExecutorWsTransport {
    runtime_id: String,
    sender: Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>,
    pending: Pending,
}

impl ExecutorWsTransport {
    /// Wrap an already-accepted WebSocket connection from the executor.
    /// Spawns a reader task that routes ToolResultEvents to pending callers.
    pub fn new(runtime_id: String, ws: WebSocketStream<TcpStream>) -> Self {
        let (sink, mut stream) = ws.split();
        let sender = Arc::new(Mutex::new(sink));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();

        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                if let Ok(msg) = serde_json::from_str::<ExecutorOutboundMessage>(&text)
                    && let Some(tx) = pending_clone.lock().await.get(&msg.request_id)
                {
                    let _ = tx.send(msg.event).await;
                }
            }
        });

        Self {
            runtime_id,
            sender,
            pending,
        }
    }

    async fn send_cmd(&self, request_id: &str, cmd: ExecutorCommand) -> Result<(), TransportError> {
        let msg = ExecutorInboundMessage {
            request_id: request_id.to_string(),
            command: cmd,
        };
        let json = serde_json::to_string(&msg)
            .map_err(|e| TransportError::Serialization(e.to_string()))?;
        self.sender
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }
}

#[async_trait]
impl RuntimeTransport for ExecutorWsTransport {
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        let (tx, mut rx) = mpsc::channel(4);
        self.pending.lock().await.insert(call_id.to_string(), tx);

        self.send_cmd(
            call_id,
            ExecutorCommand::ToolCall(ToolCallCmd {
                runtime_id: self.runtime_id.clone(),
                call: ToolCallRequest {
                    call_id: call_id.to_string(),
                    call,
                },
            }),
        )
        .await?;

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
        let request_id = Uuid::new_v4().to_string();
        self.send_cmd(
            &request_id,
            ExecutorCommand::CancelToolCall(CancelToolCallCmd {
                runtime_id: self.runtime_id.clone(),
                call_id: call_id.to_string(),
            }),
        )
        .await
    }
}
