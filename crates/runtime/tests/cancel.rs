//! A cancelled call is answered as cancelled, whatever kind of call it was.
//!
//! It used to be answered with a synthetic `ToolCallResponse` carrying
//! `Err("cancelled")` — correct for a tool call and wrong for everything else,
//! because each typed method rejects a reply of the wrong kind. Cancelling a
//! workspace scan therefore resolved its waiter with *"the runtime answered a
//! workspace scan with the wrong message"*: a protocol confusion reported in
//! place of the cancellation that actually happened.
//!
//! That was latent while only tool calls were tracked, and so only tool calls
//! could be cancelled. Now that every server-initiated command is tracked, it is
//! reachable five more ways.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::{SinkExt, StreamExt};
use horsie_models::executor::{ProvisionStep, StepParam};
use horsie_models::runtime::{
    BashInput, CancelCallRequest, ProvisionWorkspaceRequest, RuntimeInboundMessage,
    RuntimeOutboundMessage, ToolCall, ToolCallRequest,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const WINDOW: Duration = Duration::from_secs(30);

/// Long enough that the call is unambiguously still running when the cancel
/// lands — if it had already finished, the assertion would see its real reply.
const BLOCKING_TOOL: &str = "sleep 30";

#[tokio::test]
async fn a_cancelled_tool_call_is_answered_as_cancelled() {
    let ws = tempfile::tempdir().unwrap();
    let mut rt = spawn(ws.path(), "rt-cancel-tool").await;

    send(
        &mut rt.ws,
        RuntimeInboundMessage::ToolCall(ToolCallRequest {
            call_id: "c1".to_string(),
            agent_id: "a1".to_string(),
            call: ToolCall::Bash(BashInput {
                command: BLOCKING_TOOL.to_string(),
                timeout_secs: None,
            }),
        }),
    )
    .await;
    send(
        &mut rt.ws,
        RuntimeInboundMessage::CancelCall(CancelCallRequest {
            call_id: "c1".to_string(),
        }),
    )
    .await;

    match next_outbound(&mut rt.ws).await {
        RuntimeOutboundMessage::Cancelled(c) => assert_eq!(c.call_id, "c1"),
        other => panic!("expected a cancellation, got {other:?}"),
    }
}

/// The case the old shape could not express. A provision is not a tool call, so
/// a tool-shaped answer was rejected by the very method waiting for it.
#[tokio::test]
async fn a_cancelled_provision_is_answered_as_cancelled_not_as_a_tool_result() {
    let ws = tempfile::tempdir().unwrap();
    let mut rt = spawn(ws.path(), "rt-cancel-provision").await;

    // A clone that will sit in git's connect timeout rather than failing fast,
    // so the cancel lands while the step is genuinely running.
    send(
        &mut rt.ws,
        RuntimeInboundMessage::ProvisionWorkspace(ProvisionWorkspaceRequest {
            call_id: "p1".to_string(),
            steps: vec![ProvisionStep {
                name: "checkout slow".to_string(),
                uses: "git_checkout".to_string(),
                with: vec![
                    StepParam {
                        key: "url".to_string(),
                        // Reserved by RFC 5737 for documentation; never routable,
                        // so the connect hangs rather than being refused.
                        value: "https://192.0.2.1/repo.git".to_string(),
                    },
                    StepParam {
                        key: "dir".to_string(),
                        value: "repo".to_string(),
                    },
                ],
            }],
        }),
    )
    .await;
    // Let the step reach git before cancelling it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(
        &mut rt.ws,
        RuntimeInboundMessage::CancelCall(CancelCallRequest {
            call_id: "p1".to_string(),
        }),
    )
    .await;

    match next_outbound(&mut rt.ws).await {
        RuntimeOutboundMessage::Cancelled(c) => assert_eq!(c.call_id, "p1"),
        RuntimeOutboundMessage::ToolCallResponse(_) => {
            panic!("a cancelled provision must not be answered with a tool result")
        }
        other => panic!("expected a cancellation, got {other:?}"),
    }
}

/// A live runtime and its socket. The child is held so `kill_on_drop` fires
/// when the test ends rather than being leaked.
struct Runtime {
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    child: tokio::process::Child,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.child.start_kill().ok();
    }
}

async fn spawn(workspace: &Path, runtime_id: &str) -> Runtime {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let child = tokio::process::Command::new(PathBuf::from(env!("CARGO_BIN_EXE_horsie-runtime")))
        .arg("--endpoint")
        .arg(format!("ws://{addr}"))
        .arg("--runtime-id")
        .arg(runtime_id)
        .arg("--workspace")
        .arg(format!("main={}", workspace.display()))
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the runtime");

    let (stream, _) = tokio::time::timeout(WINDOW, listener.accept())
        .await
        .expect("a runtime should dial in")
        .expect("accept");
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .expect("websocket upgrade");
    let announced = next_outbound(&mut ws).await;
    assert!(
        matches!(announced, RuntimeOutboundMessage::Ready(_)),
        "expected a handshake, got {announced:?}"
    );
    Runtime { ws, child }
}

async fn send<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, message: RuntimeInboundMessage)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let json = serde_json::to_string(&message).expect("encoding a request");
    ws.send(Message::Text(json.into()))
        .await
        .expect("sending a request");
}

async fn next_outbound<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> RuntimeOutboundMessage
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let frame = tokio::time::timeout(WINDOW, ws.next())
            .await
            .expect("the runtime should answer within the window")
            .expect("the stream stays open")
            .expect("a frame");
        if let Message::Text(text) = frame {
            return serde_json::from_str(&text).expect("a decodable outbound message");
        }
    }
}
