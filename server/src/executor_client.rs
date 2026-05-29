use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand, ExecutorEvent,
    ExecutorInboundMessage, ExecutorOutboundMessage, RuntimeConfig, RuntimeState, ToolCallCmd,
};
use models::runtime::{ToolCall, ToolCallRequest, ToolOutput, ToolResult};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("tool failed: {0}")]
    ToolFailed(String),
    #[error("disconnected")]
    Disconnected,
}

#[async_trait]
pub trait ExecutorTransport: Send + Sync {
    /// Send a command; returns a channel that yields all events with this request_id.
    async fn send(
        &self,
        request_id: &str,
        cmd: ExecutorCommand,
    ) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError>;
}

/// Typed client interface to a connected executor.
pub struct ExecutorClient {
    transport: Arc<dyn ExecutorTransport>,
}

impl ExecutorClient {
    pub fn new(transport: impl ExecutorTransport + 'static) -> Self {
        Self {
            transport: Arc::new(transport),
        }
    }

    pub async fn create_runtime(&self, id: &str, config: RuntimeConfig) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(
                &req,
                ExecutorCommand::CreateRuntime(CreateRuntimeCmd {
                    runtime_id: id.to_string(),
                    config,
                }),
            )
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::RuntimeStateChanged(e)) if e.state == RuntimeState::Running => {
                    return Ok(());
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    return Err(ClientError::CommandFailed(e.message));
                }
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn destroy_runtime(&self, id: &str) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(
                &req,
                ExecutorCommand::DestroyRuntime(DestroyRuntimeCmd {
                    runtime_id: id.to_string(),
                }),
            )
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::RuntimeStateChanged(e)) if e.state == RuntimeState::Stopped => {
                    return Ok(());
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    return Err(ClientError::CommandFailed(e.message));
                }
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn invoke_tool(
        &self,
        runtime_id: &str,
        call: ToolCall,
    ) -> Result<ToolOutput, ClientError> {
        let call_id = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(
                &call_id,
                ExecutorCommand::ToolCall(ToolCallCmd {
                    runtime_id: runtime_id.to_string(),
                    call: ToolCallRequest {
                        call_id: call_id.clone(),
                        call,
                    },
                }),
            )
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::ToolResult(ev)) if ev.call_id == call_id => {
                    return match ev.result {
                        ToolResult::Ok(o) => Ok(o),
                        ToolResult::Err(e) => Err(ClientError::ToolFailed(e.reason)),
                    };
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    return Err(ClientError::CommandFailed(e.message));
                }
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn cancel_tool_call(
        &self,
        runtime_id: &str,
        call_id: &str,
    ) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let _rx = self
            .transport
            .send(
                &req,
                ExecutorCommand::CancelToolCall(CancelToolCallCmd {
                    runtime_id: runtime_id.to_string(),
                    call_id: call_id.to_string(),
                }),
            )
            .await?;
        // Fire and forget — result comes back as ToolResult on the original invoke_tool call
        Ok(())
    }
}

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<ExecutorEvent>>>>;

/// WS transport wrapping the server side of an executor connection.
pub struct WsExecutorTransport {
    sender: Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>,
    pending: Pending,
}

impl WsExecutorTransport {
    /// Wrap an accepted WebSocket stream (server side of the executor's outbound connection).
    /// Consumes the first Registered event, then routes subsequent events by request_id.
    pub async fn accept(ws: WebSocketStream<TcpStream>) -> Self {
        let (sink, mut stream) = ws.split();
        let sender = Arc::new(Mutex::new(sink));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();

        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                if let Ok(msg) = serde_json::from_str::<ExecutorOutboundMessage>(&text) {
                    // Skip the Registered handshake event
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

#[async_trait]
impl ExecutorTransport for WsExecutorTransport {
    async fn send(
        &self,
        request_id: &str,
        cmd: ExecutorCommand,
    ) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError> {
        let (tx, rx) = mpsc::channel(16);
        self.pending.lock().await.insert(request_id.to_string(), tx);

        let msg = ExecutorInboundMessage {
            request_id: request_id.to_string(),
            command: cmd,
        };
        let json =
            serde_json::to_string(&msg).map_err(|e| ClientError::Serialization(e.to_string()))?;
        self.sender
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| ClientError::SendFailed(e.to_string()))?;

        Ok(rx)
    }
}
