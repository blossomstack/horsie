//! `horsie connect` runs this machine as a runtime vendor agent.
//!
//! This drives the whole chain against a fake session server that speaks the
//! real `vendor.fl` protocol: the agent dials in and announces itself, a
//! `CreateRuntime` makes it spawn a real `horsie-runtime` child that dials the
//! agent's own unix socket, and a `ToolCall` is relayed to that child and
//! answered. Nothing is stubbed between the CLI and the runtime binary.
//!
//! `horsie-runtime` isn't a build dependency of `cli` (see
//! `cli/src/daemon/mod.rs`'s `default_runtime_bin` — the CLI finds it as a
//! sibling *file* at runtime, not a linked crate), so there's no
//! `CARGO_BIN_EXE_horsie-runtime`. `locate_runtime_bin` mirrors the
//! relative-path search `cli/tests/sandbox_e2e.rs` already uses. Only built
//! when the workspace has been compiled — skip, don't fail, if it's absent.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime::{
    BashInput, RuntimeInboundMessage, RuntimeOutboundMessage, ToolCall, ToolCallRequest, ToolResult,
};
use horsie_models::runtime_vendor::{
    CreateRuntimeRequest, RuntimeRelayRequest, RuntimeSpec, RuntimeVendorCommand,
    RuntimeVendorEvent, RuntimeVendorInboundMessage, RuntimeVendorOutboundMessage,
};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

fn locate_runtime_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?; // .../target/<profile>/deps
    if let Some(profile) = dir.parent() {
        let cand = profile.join("horsie-runtime");
        if cand.exists() {
            return Some(cand);
        }
    }
    let cand = dir.join("horsie-runtime");
    cand.exists().then_some(cand)
}

/// A minimal stand-in for the server's `/api/vendor/connect`: accept one TCP
/// connection, complete the WebSocket handshake by hand, and hand back the
/// framed stream.
async fn accept_vendor_agent(listener: TcpListener) -> WebSocketStream<tokio::net::TcpStream> {
    let (stream, _) = listener.accept().await.expect("accept agent");
    tokio_tungstenite::accept_async(stream)
        .await
        .expect("websocket handshake")
}

async fn next_event(
    ws: &mut WebSocketStream<tokio::net::TcpStream>,
) -> RuntimeVendorOutboundMessage {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(30), ws.next())
            .await
            .expect("agent went quiet")
            .expect("stream open")
            .expect("frame");
        if let Message::Text(text) = msg {
            return serde_json::from_str(&text).expect("decode event");
        }
    }
}

async fn send_command(
    ws: &mut WebSocketStream<tokio::net::TcpStream>,
    request_id: &str,
    command: RuntimeVendorCommand,
) {
    let msg = RuntimeVendorInboundMessage {
        request_id: request_id.to_string(),
        command,
    };
    ws.send(Message::Text(serde_json::to_string(&msg).unwrap().into()))
        .await
        .expect("send command");
}

#[tokio::test]
async fn connect_registers_as_a_vendor_then_spawns_and_serves_a_runtime() {
    let Some(runtime_bin) = locate_runtime_bin() else {
        eprintln!(
            "skipping connect_registers_as_a_vendor_then_spawns_and_serves_a_runtime: \
             horsie-runtime binary not found (run via `cargo test --workspace`)"
        );
        return;
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(accept_vendor_agent(listener));

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("marker.txt"), "hello").unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{"runtime": {{"bin": {:?}}}, "storage": {{"state_dir": {:?}}}}}"#,
            runtime_bin,
            config_dir.path().join("state"),
        ),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_horsie"))
        .args([
            "connect",
            "--server",
            &format!("http://{addr}"),
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--name",
            "test-vendor",
            "--config",
        ])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn horsie connect");

    let mut ws = server.await.expect("server task");

    // 1. The agent announces itself under the name we gave it.
    let boot = next_event(&mut ws).await;
    match boot.event {
        RuntimeVendorEvent::Ready(ev) => {
            assert_eq!(ev.vendor_name, "test-vendor");
            assert!(
                !ev.capabilities.supports_provisioning,
                "a fixed user directory provisions nothing"
            );
        }
        other => panic!("expected Ready, got {other:?}"),
    }

    // 2. CreateRuntime spawns a real horsie-runtime that dials the agent's own
    //    unix socket. The agent only answers once that dial-back has landed.
    send_command(
        &mut ws,
        "req-create",
        RuntimeVendorCommand::CreateRuntime(CreateRuntimeRequest {
            runtime_id: "rt-1".to_string(),
            spec: RuntimeSpec {
                workspaces: vec!["main".to_string()],
                env: vec![],
                provision: vec![],
                sandbox_capabilities: None,
            },
        }),
    )
    .await;
    let created = next_event(&mut ws).await;
    assert_eq!(created.request_id, "req-create");
    match created.event {
        RuntimeVendorEvent::CreateRuntime(ev) => assert_eq!(ev.runtime_id, "rt-1"),
        other => panic!("expected the runtime to come up, got {other:?}"),
    }

    // 3. A tool call is relayed to that child and answered.
    send_command(
        &mut ws,
        "req-tool",
        RuntimeVendorCommand::Runtime(RuntimeRelayRequest {
            runtime_id: "rt-1".to_string(),
            message: RuntimeInboundMessage::ToolCall(ToolCallRequest {
                call_id: "call-1".to_string(),
                call: ToolCall::Bash(BashInput {
                    command: "cat marker.txt".to_string(),
                    timeout_secs: None,
                    workspace: None,
                }),
            }),
        }),
    )
    .await;
    let tooled = next_event(&mut ws).await;
    assert_eq!(tooled.request_id, "req-tool");
    match tooled.event {
        RuntimeVendorEvent::Runtime(ev) => match ev.message {
            RuntimeOutboundMessage::ToolCallResponse(resp) => match resp.result {
                ToolResult::Ok(out) => assert!(
                    out.stdout.contains("hello"),
                    "the tool ran in the configured workspace, got {out:?}"
                ),
                ToolResult::Err(e) => panic!("tool call failed: {}", e.reason),
            },
            other => panic!("expected a tool result, got {other:?}"),
        },
        other => panic!("expected a relayed runtime reply, got {other:?}"),
    }

    // 4. An unknown workspace name fails explicitly rather than silently
    //    substituting a directory.
    send_command(
        &mut ws,
        "req-bad",
        RuntimeVendorCommand::CreateRuntime(CreateRuntimeRequest {
            runtime_id: "rt-2".to_string(),
            spec: RuntimeSpec {
                workspaces: vec!["nope".to_string()],
                env: vec![],
                provision: vec![],
                sandbox_capabilities: None,
            },
        }),
    )
    .await;
    let refused = next_event(&mut ws).await;
    assert_eq!(refused.request_id, "req-bad");
    match refused.event {
        RuntimeVendorEvent::RequestFailed(ev) => assert!(
            ev.message.contains("nope"),
            "the failure must name the workspace, got {}",
            ev.message
        ),
        other => panic!("expected RequestFailed, got {other:?}"),
    }

    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[tokio::test]
async fn connect_rejects_background_with_a_pointer_to_a_process_manager() {
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{"storage": {{"state_dir": {:?}}}}}"#,
            config_dir.path().join("state"),
        ),
    )
    .unwrap();
    let workspace = tempfile::tempdir().unwrap();

    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_horsie"))
        .args([
            "connect",
            "--server",
            "http://127.0.0.1:1",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--background",
            "--config",
        ])
        .arg(&config_path)
        .output()
        .await
        .expect("run horsie connect");

    assert!(!out.status.success(), "--background must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("process manager"),
        "the error should point at a process manager, got: {stderr}"
    );
}

/// When the agent goes away, so must the runtimes it spawned.
///
/// `tokio::process::Child` does not kill on drop, so an agent that simply
/// returned from its loop would leave one orphaned `horsie-runtime` per live
/// session holding the user's workspace open.
#[tokio::test]
#[cfg(unix)]
async fn runtimes_die_with_the_agent() {
    let Some(runtime_bin) = locate_runtime_bin() else {
        eprintln!("skipping runtimes_die_with_the_agent: horsie-runtime binary not found");
        return;
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(accept_vendor_agent(listener));

    // A uniquely-named workspace dir, so pgrep can find exactly our runtime.
    let workspace = tempfile::tempdir().unwrap();
    let marker_dir = workspace.path().join("horsie-orphan-probe");
    std::fs::create_dir_all(&marker_dir).unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{"runtime": {{"bin": {:?}}}, "storage": {{"state_dir": {:?}}}}}"#,
            runtime_bin,
            config_dir.path().join("state"),
        ),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_horsie"))
        .args([
            "connect",
            "--server",
            &format!("http://{addr}"),
            "--workspace",
            marker_dir.to_str().unwrap(),
            "--name",
            "orphan-probe",
            "--config",
        ])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn horsie connect");

    let mut ws = server.await.expect("server task");
    let _boot = next_event(&mut ws).await;
    send_command(
        &mut ws,
        "req-create",
        RuntimeVendorCommand::CreateRuntime(CreateRuntimeRequest {
            runtime_id: "rt-1".to_string(),
            spec: RuntimeSpec {
                workspaces: vec!["main".to_string()],
                env: vec![],
                provision: vec![],
                sandbox_capabilities: None,
            },
        }),
    )
    .await;
    let _created = next_event(&mut ws).await;

    let probe = marker_dir.to_str().unwrap().to_string();
    let running = || {
        std::process::Command::new("pgrep")
            .args(["-f", &probe])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .count()
            })
            .unwrap_or(0)
    };
    assert!(
        running() > 0,
        "the runtime should be up before we cut the link"
    );

    // Cut the server link: the agent's read loop ends and it shuts down.
    drop(ws);
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let _ = child.kill().await;

    let mut gone = false;
    for _ in 0..150 {
        if running() == 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if !gone {
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-f", &probe])
            .status();
    }
    assert!(gone, "the agent orphaned its runtime child");
}
