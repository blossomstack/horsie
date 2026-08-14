//! A runtime answers a ping while it is busy, and says what it is busy with.
//!
//! Both halves are load-bearing, and the first is the one that cannot be
//! compromised on. The caller fails **every** outstanding call against a runtime
//! that does not answer a ping inside its window, so a ping queued behind a
//! running tool would abort exactly the long work reconciliation exists to
//! protect: a twenty-minute build would be declared dead nineteen minutes early,
//! every time.
//!
//! The second half is what makes liveness usable at all. A boolean "yes I am
//! alive" cannot tell a build that is still running from a request the runtime
//! never received, and those need opposite responses — wait indefinitely, or
//! fail. The list of ids is what separates them.
//!
//! Against the real binary over a real socket, because concurrency here is a
//! property of the dispatcher and a mock would only re-assert its own shape.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime::{
    BashInput, PingRequest, RuntimeInboundMessage, RuntimeOutboundMessage, ToolCall,
    ToolCallRequest,
};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Generous enough to cover a process spawn and a websocket upgrade, short
/// enough that a serialised dispatcher fails the suite rather than stalling it.
const WINDOW: Duration = Duration::from_secs(20);

/// Long enough that the tool is unambiguously still running when the pong
/// arrives — if the ping were queued behind it, the assertion below would time
/// out rather than pass late.
const BLOCKING_TOOL: &str = "sleep 30";

#[tokio::test]
async fn a_busy_runtime_answers_a_ping_and_names_the_call_it_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    // Port 0: the OS picks, so the test never collides with a parallel run.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mut child =
        tokio::process::Command::new(PathBuf::from(env!("CARGO_BIN_EXE_horsie-runtime")))
            .arg("--endpoint")
            .arg(format!("ws://{addr}"))
            .arg("--runtime-id")
            .arg("rt-ping")
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

    // Its handshake, so the dispatcher loop is running before anything is sent.
    let announced = next_outbound(&mut ws).await;
    assert!(
        matches!(announced, RuntimeOutboundMessage::Ready(_)),
        "expected a handshake, got {announced:?}"
    );

    send(
        &mut ws,
        RuntimeInboundMessage::ToolCall(ToolCallRequest {
            call_id: "the-long-one".to_string(),
            agent_id: "a1".to_string(),
            call: ToolCall::Bash(BashInput {
                command: BLOCKING_TOOL.to_string(),
                timeout_secs: None,
            }),
        }),
    )
    .await;

    send(
        &mut ws,
        RuntimeInboundMessage::Ping(PingRequest {
            call_id: "p1".to_string(),
        }),
    )
    .await;

    // The pong has to arrive *before* the tool's own response, which is the
    // whole assertion: the reply that comes back first proves the dispatcher did
    // not queue the ping behind a thirty-second sleep.
    let reply = next_outbound(&mut ws).await;
    let RuntimeOutboundMessage::Pong(pong) = reply else {
        panic!("a ping must be answered before the tool it overtakes, got {reply:?}");
    };
    assert_eq!(pong.call_id, "p1", "a pong answers its own ping");
    assert_eq!(
        pong.in_flight,
        vec!["the-long-one".to_string()],
        "a pong names what the runtime is executing, and nothing else — a ping \
         that counted itself would make every runtime look permanently busy"
    );

    child.kill().await.ok();
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

/// The next decodable outbound message, skipping the pings and pongs the
/// websocket layer exchanges on its own.
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
