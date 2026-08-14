//! `horsie connect` runs this machine as a runtime vendor process.
//!
//! This drives the whole chain against a fake session server that speaks the
//! real `vendor.fl` protocol: the agent dials in and announces itself, a
//! `CreateRuntime` makes it spawn a real `horsie-runtime` child that dials the
//! agent's own unix socket, and a `ToolCall` is relayed to that child and
//! answered. Nothing is stubbed between the CLI and the runtime binary.
//!
//! `horsie-runtime` isn't a build dependency of `cli` (see
//! `cli/src/connect.rs`'s `default_runtime_bin` — the CLI finds it as a
//! sibling *file* at runtime, not a linked crate), so there's no
//! `CARGO_BIN_EXE_horsie-runtime`. `locate_runtime_bin` mirrors the
//! relative-path search the daemon-era sandbox e2e used. Only built
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
    VendorRegistered,
};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// The dial token the *server* mints for a runtime and ships in `RuntimeSpec.env`.
///
/// This test stands in for that server, so it has to mint too: the vendor files
/// whatever token the spec carries and its listener refuses any dial-back it
/// cannot find in that table. A spec with no token produces a runtime that
/// comes up, dials, and is turned away — which surfaces as a connect timeout.
fn dial_env(runtime_id: &str) -> Vec<horsie_models::executor::EnvVar> {
    vec![horsie_models::executor::EnvVar {
        name: horsie_models::ENV_CONNECT_TOKEN.to_string(),
        value: format!("dial-token-for-{runtime_id}"),
    }]
}

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

/// A minimal stand-in for the server's `/api/vendor/connect`: accept TCP
/// connections until one is a WebSocket upgrade, complete the handshake by
/// hand, and hand back the framed stream.
///
/// Plain HTTP requests are answered and skipped rather than treated as a failed
/// upgrade: `horsie connect` probes `/api/auth/status` before dialing, and a
/// stand-in that panicked on it would be testing the harness, not the agent.
async fn accept_vendor_agent(listener: TcpListener) -> WebSocketStream<tokio::net::TcpStream> {
    loop {
        let (mut stream, _) = listener.accept().await.expect("accept agent");
        let mut peek = [0u8; 1024];
        let n = stream.peek(&mut peek).await.expect("peek");
        let head = String::from_utf8_lossy(&peek[..n]);
        if head.to_ascii_lowercase().contains("upgrade: websocket") {
            return tokio_tungstenite::accept_async(stream)
                .await
                .expect("websocket handshake");
        }
        // Answer as a server with authentication disabled, then wait for the
        // real dial.
        let body = br#"{"enabled":false,"authenticated":false,"mustChangePassword":false}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.shutdown().await;
    }
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

/// Answer the handshake the way the real server does. The agent waits for this
/// before it serves anything, so a stand-in that stayed silent would look like a
/// server that never published it.
async fn confirm_registration(
    ws: &mut WebSocketStream<tokio::net::TcpStream>,
    boot: &RuntimeVendorOutboundMessage,
) {
    send_command(
        ws,
        &boot.request_id,
        RuntimeVendorCommand::VendorRegistered(VendorRegistered {}),
    )
    .await;
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
            // These tests exercise the vendor chain, not the sandbox, and must
            // not depend on nono support on the host running them.
            "--no-sandbox",
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
    confirm_registration(&mut ws, &boot).await;
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
                env: dial_env("rt-1"),
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
                agent_id: "agent-1".to_string(),
                call: ToolCall::Bash(BashInput {
                    command: "cat marker.txt".to_string(),
                    timeout_secs: None,
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
                env: dial_env("rt-2"),
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

/// Restarting the agent must not cost the user their sessions.
///
/// The whole chain, twice over the same state directory: create a runtime, give
/// its agent some state a session would care about, kill the process, start
/// another one, and require that a `GetRuntime` rebuilds it with that state
/// intact. Before this, the second agent answered `Gone` and the server wrote
/// the session off permanently.
#[tokio::test]
async fn a_runtime_survives_restarting_the_agent() {
    let Some(runtime_bin) = locate_runtime_bin() else {
        eprintln!("skipping a_runtime_survives_restarting_the_agent: horsie-runtime not found");
        return;
    };

    let workspace = tempfile::tempdir().unwrap();
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

    // Both incarnations share the state dir and the workspace — the two things
    // that outlive the process on a real machine.
    let spawn_agent = |addr: std::net::SocketAddr| {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_horsie"))
            .args([
                "connect",
                "--server",
                &format!("http://{addr}"),
                "--workspace",
                workspace.path().to_str().unwrap(),
                "--name",
                "test-vendor",
                "--no-sandbox",
                "--config",
            ])
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn horsie connect")
    };

    // ── first incarnation ────────────────────────────────────────────────────
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(accept_vendor_agent(listener));
    let mut first = spawn_agent(addr);
    let mut ws = server.await.expect("server task");
    let boot = next_event(&mut ws).await;
    confirm_registration(&mut ws, &boot).await;

    send_command(
        &mut ws,
        "req-create",
        RuntimeVendorCommand::CreateRuntime(CreateRuntimeRequest {
            runtime_id: "rt-1".to_string(),
            spec: RuntimeSpec {
                workspaces: vec!["main".to_string()],
                env: dial_env("rt-1"),
            },
        }),
    )
    .await;
    let created = next_event(&mut ws).await;
    match created.event {
        RuntimeVendorEvent::CreateRuntime(ev) => assert_eq!(ev.runtime_id, "rt-1"),
        other => panic!("expected the runtime to come up, got {other:?}"),
    }

    // State a session would notice losing.
    send_command(
        &mut ws,
        "req-setenv",
        RuntimeVendorCommand::Runtime(RuntimeRelayRequest {
            runtime_id: "rt-1".to_string(),
            message: RuntimeInboundMessage::ToolCall(ToolCallRequest {
                call_id: "call-env".to_string(),
                agent_id: "agent-1".to_string(),
                call: ToolCall::SetEnv(horsie_models::runtime::SetEnvInput {
                    name: "SURVIVES".to_string(),
                    value: Some("yes".to_string()),
                }),
            }),
        }),
    )
    .await;
    let _ = next_event(&mut ws).await;

    let _ = first.kill().await;
    let _ = first.wait().await;

    // ── second incarnation, same machine ─────────────────────────────────────
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(accept_vendor_agent(listener));
    let mut second = spawn_agent(addr);
    let mut ws = server.await.expect("server task");
    let boot = next_event(&mut ws).await;
    confirm_registration(&mut ws, &boot).await;

    send_command(
        &mut ws,
        "req-get",
        RuntimeVendorCommand::GetRuntime(horsie_models::runtime_vendor::GetRuntimeRequest {
            runtime_id: "rt-1".to_string(),
            // The same spec the create carried: the agent keeps no copy, so
            // this is the only description of the runtime it has. The token is
            // re-minted rather than remembered — this agent is a fresh process
            // whose issued-token table starts empty, so a get that carried no
            // token could never revive anything.
            spec: RuntimeSpec {
                workspaces: vec!["main".to_string()],
                env: dial_env("rt-1"),
            },
        }),
    )
    .await;
    let got = next_event(&mut ws).await;
    match got.event {
        RuntimeVendorEvent::GetRuntime(ev) => assert_eq!(ev.runtime_id, "rt-1"),
        other => panic!("a get after a restart must rebuild the runtime, got {other:?}"),
    }

    // And the rebuilt runtime is the same one as far as the agent is concerned.
    send_command(
        &mut ws,
        "req-tool",
        RuntimeVendorCommand::Runtime(RuntimeRelayRequest {
            runtime_id: "rt-1".to_string(),
            message: RuntimeInboundMessage::ToolCall(ToolCallRequest {
                call_id: "call-1".to_string(),
                agent_id: "agent-1".to_string(),
                call: ToolCall::Bash(BashInput {
                    command: "echo $SURVIVES".to_string(),
                    timeout_secs: None,
                }),
            }),
        }),
    )
    .await;
    let tooled = next_event(&mut ws).await;
    match tooled.event {
        RuntimeVendorEvent::Runtime(ev) => match ev.message {
            RuntimeOutboundMessage::ToolCallResponse(resp) => match resp.result {
                ToolResult::Ok(out) => assert!(
                    out.stdout.contains("yes"),
                    "the agent's env overlay must survive the restart, got {out:?}"
                ),
                ToolResult::Err(e) => panic!("tool call failed: {}", e.reason),
            },
            other => panic!("expected a tool result, got {other:?}"),
        },
        other => panic!("expected a relayed runtime reply, got {other:?}"),
    }

    let _ = second.kill().await;
    let _ = second.wait().await;
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
            "--no-sandbox",
            "--config",
        ])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn horsie connect");

    let mut ws = server.await.expect("server task");
    let boot = next_event(&mut ws).await;
    confirm_registration(&mut ws, &boot).await;
    send_command(
        &mut ws,
        "req-create",
        RuntimeVendorCommand::CreateRuntime(CreateRuntimeRequest {
            runtime_id: "rt-1".to_string(),
            spec: RuntimeSpec {
                workspaces: vec!["main".to_string()],
                env: dial_env("rt-1"),
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
