use crate::{
    error::ServerError,
    handler::ExecutorEventHandler,
    registry::{CommandSink, ExecutorConn, ExecutorRegistry},
};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand, ExecutorInboundMessage,
    ExecutorOutboundMessage, QueryRuntimesCmd, RestartRuntimeCmd, RuntimeConfig,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};
use uuid::Uuid;

type WsSink = Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>;

struct WsCommandSink {
    sink: WsSink,
}

#[async_trait]
impl CommandSink for WsCommandSink {
    async fn send(&self, msg: ExecutorInboundMessage) -> Result<(), ServerError> {
        let json =
            serde_json::to_string(&msg).map_err(|e| ServerError::Serialization(e.to_string()))?;
        self.sink
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| ServerError::SendFailed(e.to_string()))
    }
}

pub struct Server {
    registry: Arc<ExecutorRegistry>,
    local_addr: SocketAddr,
}

impl Server {
    pub async fn bind(
        addr: &str,
        handler: Arc<dyn ExecutorEventHandler>,
    ) -> Result<Self, ServerError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| ServerError::BindFailed(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ServerError::BindFailed(e.to_string()))?;
        let registry = Arc::new(ExecutorRegistry::new());
        let reg = registry.clone();
        tokio::spawn(async move { accept_loop(listener, reg, handler).await });
        Ok(Self {
            registry,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn create_runtime(
        &self,
        executor_id: &str,
        runtime_id: &str,
        config: RuntimeConfig,
    ) -> Result<(), ServerError> {
        self.registry
            .send_command(
                executor_id,
                Uuid::new_v4().to_string(),
                ExecutorCommand::CreateRuntime(CreateRuntimeCmd {
                    runtime_id: runtime_id.to_string(),
                    config,
                }),
            )
            .await
    }

    pub async fn destroy_runtime(
        &self,
        executor_id: &str,
        runtime_id: &str,
    ) -> Result<(), ServerError> {
        self.registry
            .send_command(
                executor_id,
                Uuid::new_v4().to_string(),
                ExecutorCommand::DestroyRuntime(DestroyRuntimeCmd {
                    runtime_id: runtime_id.to_string(),
                }),
            )
            .await
    }

    pub async fn restart_runtime(
        &self,
        executor_id: &str,
        runtime_id: &str,
    ) -> Result<(), ServerError> {
        self.registry
            .send_command(
                executor_id,
                Uuid::new_v4().to_string(),
                ExecutorCommand::RestartRuntime(RestartRuntimeCmd {
                    runtime_id: runtime_id.to_string(),
                }),
            )
            .await
    }

    pub async fn query_runtimes(&self, executor_id: &str) -> Result<(), ServerError> {
        self.registry
            .send_command(
                executor_id,
                Uuid::new_v4().to_string(),
                ExecutorCommand::QueryRuntimes(QueryRuntimesCmd {}),
            )
            .await
    }
}

async fn accept_loop(
    listener: TcpListener,
    registry: Arc<ExecutorRegistry>,
    handler: Arc<dyn ExecutorEventHandler>,
) {
    while let Ok((stream, _)) = listener.accept().await {
        let reg = registry.clone();
        let hdl = handler.clone();
        tokio::spawn(async move { handle_connection(stream, reg, hdl).await });
    }
}

async fn handle_connection(
    stream: TcpStream,
    registry: Arc<ExecutorRegistry>,
    handler: Arc<dyn ExecutorEventHandler>,
) {
    let ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let (sink, mut stream) = ws.split();
    let ws_sink: WsSink = Arc::new(Mutex::new(sink));

    // First message must be Registered
    let executor_id = loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(msg) = serde_json::from_str::<ExecutorOutboundMessage>(&text)
                    && let models::executor::ExecutorEvent::Registered(ref ev) = msg.event
                {
                    break ev.executor_id.clone();
                }
            }
            _ => return,
        }
    };

    let conn = Arc::new(ExecutorConn::new(
        executor_id.clone(),
        Arc::new(WsCommandSink { sink: ws_sink }),
    ));
    registry.register(conn).await;

    while let Some(msg_result) = stream.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                if let Ok(outbound) = serde_json::from_str::<ExecutorOutboundMessage>(&text) {
                    handler.on_event(&executor_id, &outbound.request_id, &outbound.event);
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(Message::Binary(_))
            | Ok(Message::Ping(_))
            | Ok(Message::Pong(_))
            | Ok(Message::Frame(_)) => {}
        }
    }

    registry.remove(&executor_id).await;
}
