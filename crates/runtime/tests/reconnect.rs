//! A runtime outlives its link.
//!
//! The real `horsie-runtime` binary dials a socket this test owns, so the test
//! can drop the connection out from under it and watch it come back. That is
//! the whole behaviour: before the reconnect loop, a dropped frame ended the
//! process and took the workspace with it — and for a vendor that hibernates by
//! stopping a machine, meant a runtime could never be resumed, only rebuilt.
//!
//! Over TCP deliberately. A unix endpoint belongs to the vendor process that
//! spawned this runtime as its child, so there a dropped link means the parent
//! is gone and exiting is correct — `connect_e2e` covers that half.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::StreamExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Long enough to cover a process spawn plus the first backoff (1s), short
/// enough that a regression fails the suite rather than stalling it.
const WINDOW: Duration = Duration::from_secs(20);

/// Accept one connection and return the first text frame the runtime sends,
/// then drop the socket. Dropping it is the point: that is the disconnect.
async fn accept_one_announcement(listener: &TcpListener) -> String {
    let (stream, _) = tokio::time::timeout(WINDOW, listener.accept())
        .await
        .expect("a runtime should dial in")
        .expect("accept");
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .expect("websocket upgrade");
    loop {
        let msg = tokio::time::timeout(WINDOW, ws.next())
            .await
            .expect("the runtime should announce itself")
            .expect("stream open")
            .expect("frame");
        if let Message::Text(text) = msg {
            return text.to_string();
        }
    }
}

#[tokio::test]
async fn a_runtime_redials_after_its_link_drops() {
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
            .arg("rt-reconnect")
            .arg("--workspace")
            .arg(format!("main={}", workspace.display()))
            .kill_on_drop(true)
            .spawn()
            .expect("spawn the runtime");

    let first = accept_one_announcement(&listener).await;
    assert!(first.contains("rt-reconnect"), "got: {first}");

    // The listener is still bound, so the re-dial has somewhere to land — the
    // runtime is reconnecting to a live server, which is the ordinary case
    // (a restart, a resumed machine, a network blink).
    let second = accept_one_announcement(&listener).await;
    assert!(
        second.contains("rt-reconnect"),
        "a runtime must re-announce itself on a new link, got: {second}"
    );

    child.kill().await.ok();
}
