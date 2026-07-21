use crate::client::ClientError;
use crate::transport::ExecutorTransport;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use horsie_models::executor::{
    CancelToolCallCmd, ExecutorCommand, ExecutorEvent, ExecutorInboundMessage,
    ExecutorOutboundMessage, ScanWorkspaceCmd, SessionStartCmd, ToolCallCmd,
};
use horsie_models::runtime::{
    PluginSkill, ScanRequest, SessionStartRequest, ToolCall, ToolCallRequest, ToolResult,
    WorkspaceScan,
};
use horsie_runtime_client::{RuntimeTransport, TransportError};
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

/// Runtime transport that relays through the executor over the shared
/// client↔executor WS connection (server / distributed mode). Tool calls,
/// workspace scans, and SessionStart hooks all ride this one socket.
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
                // Executor-side failure (e.g. runtime not connected): surface it
                // instead of hanging on a reply that will never come.
                Some(ExecutorEvent::CommandFailed(e)) => {
                    self.pending.lock().await.remove(call_id);
                    return Err(TransportError::SendFailed(e.message));
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

    async fn scan_workspace(
        &self,
        call_id: &str,
        workspace: Option<String>,
        instruction_candidates: Vec<String>,
        skills_glob: String,
        include_shared: bool,
    ) -> Result<(Vec<WorkspaceScan>, Vec<PluginSkill>), TransportError> {
        let mut rx = send_command(
            &self.sender,
            &self.pending,
            call_id,
            ExecutorCommand::ScanWorkspace(ScanWorkspaceCmd {
                runtime_id: self.runtime_id.clone(),
                request: ScanRequest {
                    call_id: call_id.to_string(),
                    workspace,
                    instruction_candidates,
                    skills_glob,
                    include_shared,
                },
            }),
        )
        .await
        .map_err(|e| TransportError::SendFailed(e.to_string()))?;

        loop {
            match rx.recv().await {
                Some(ExecutorEvent::ScanResult(ev)) if ev.response.call_id == call_id => {
                    self.pending.lock().await.remove(call_id);
                    return Ok((ev.response.workspaces, ev.response.shared_skills));
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    self.pending.lock().await.remove(call_id);
                    return Err(TransportError::SendFailed(e.message));
                }
                Some(_) => continue,
                None => {
                    self.pending.lock().await.remove(call_id);
                    return Err(TransportError::Disconnected);
                }
            }
        }
    }

    async fn run_session_start(&self, call_id: &str) -> Result<String, TransportError> {
        let mut rx = send_command(
            &self.sender,
            &self.pending,
            call_id,
            ExecutorCommand::SessionStart(SessionStartCmd {
                runtime_id: self.runtime_id.clone(),
                request: SessionStartRequest {
                    call_id: call_id.to_string(),
                },
            }),
        )
        .await
        .map_err(|e| TransportError::SendFailed(e.to_string()))?;

        loop {
            match rx.recv().await {
                Some(ExecutorEvent::SessionStartResult(ev)) if ev.response.call_id == call_id => {
                    self.pending.lock().await.remove(call_id);
                    return Ok(ev.response.context);
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    self.pending.lock().await.remove(call_id);
                    return Err(TransportError::SendFailed(e.message));
                }
                Some(_) => continue,
                None => {
                    self.pending.lock().await.remove(call_id);
                    return Err(TransportError::Disconnected);
                }
            }
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
    use horsie_models::executor::{
        CommandFailedEvent, RegisteredEvent, ScanResultEvent, SessionStartResultEvent,
    };
    use horsie_models::runtime::{ScanResponse, ScannedFile, SessionStartResponse};

    /// A fake executor: answers ScanWorkspace with an AGENTS.md scan, SessionStart
    /// with a bootstrap context, and (on request) everything with CommandFailed.
    async fn fake_executor(listener: tokio::net::TcpListener, fail_all: bool) {
        let (tcp, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        let registered = ExecutorOutboundMessage {
            request_id: Uuid::new_v4().to_string(),
            event: ExecutorEvent::Registered(RegisteredEvent {
                executor_id: "ex-1".into(),
            }),
        };
        sink.send(Message::Text(
            serde_json::to_string(&registered).unwrap().into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(Message::Text(t))) = stream.next().await {
            let msg: ExecutorInboundMessage = serde_json::from_str(&t).unwrap();
            let event = if fail_all {
                ExecutorEvent::CommandFailed(CommandFailedEvent {
                    message: "runtime 'rt-1' not connected".into(),
                })
            } else {
                match msg.command {
                    ExecutorCommand::ScanWorkspace(cmd) => {
                        ExecutorEvent::ScanResult(ScanResultEvent {
                            runtime_id: cmd.runtime_id,
                            response: ScanResponse {
                                call_id: cmd.request.call_id,
                                workspaces: vec![WorkspaceScan {
                                    name: "app".into(),
                                    path: "/ws/app".into(),
                                    is_git_repo: true,
                                    instructions: Some(ScannedFile {
                                        path: "AGENTS.md".into(),
                                        content: "ctx".into(),
                                    }),
                                    skills: vec![],
                                }],
                                shared_skills: vec![],
                            },
                        })
                    }
                    ExecutorCommand::SessionStart(cmd) => {
                        ExecutorEvent::SessionStartResult(SessionStartResultEvent {
                            runtime_id: cmd.runtime_id,
                            response: SessionStartResponse {
                                call_id: cmd.request.call_id,
                                context: "boot".into(),
                            },
                        })
                    }
                    other => panic!("unexpected command: {other:?}"),
                }
            };
            let reply = ExecutorOutboundMessage {
                request_id: msg.request_id,
                event,
            };
            sink.send(Message::Text(serde_json::to_string(&reply).unwrap().into()))
                .await
                .unwrap();
        }
    }

    async fn relay_to_fake_executor(fail_all: bool) -> Arc<dyn RuntimeTransport> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(fake_executor(listener, fail_all));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (ws, _) = tokio_tungstenite::client_async("ws://localhost/", tcp)
            .await
            .unwrap();
        let transport = WsExecutorTransport::accept(ws);
        transport.runtime_transport("rt-1").await.unwrap()
    }

    #[tokio::test]
    async fn scan_and_session_start_relay_over_the_executor_link() {
        let relay = relay_to_fake_executor(false).await;
        let (workspaces, shared) = relay
            .scan_workspace(
                "s1",
                None,
                vec!["AGENTS.md".into()],
                ".claude/skills/*/SKILL.md".into(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(workspaces[0].instructions.as_ref().unwrap().content, "ctx");
        assert!(shared.is_empty());

        let context = relay.run_session_start("ss1").await.unwrap();
        assert_eq!(context, "boot");
    }

    #[tokio::test]
    async fn executor_side_failure_surfaces_instead_of_hanging() {
        let relay = relay_to_fake_executor(true).await;
        let err = relay
            .scan_workspace("s1", None, vec![], "glob".into(), false)
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::SendFailed(_)));
        let err = relay.run_session_start("ss1").await.unwrap_err();
        assert!(matches!(err, TransportError::SendFailed(_)));
    }
}
