//! `horsie connect` spawns a real `horsie-runtime` binary that dials a real
//! (fake) session-server listener and announces itself under the given
//! runtime id — the same wire behavior a real session server's
//! `/api/runtime/connect` endpoint expects.
//!
//! `horsie-runtime`'s binary isn't a build dependency of `cli` (see
//! `cli/src/daemon/mod.rs`'s `default_runtime_bin` — the CLI finds it as a
//! sibling *file* at runtime, not a linked crate), so there's no
//! `CARGO_BIN_EXE_horsie-runtime` for this test to use. `locate_runtime_bin`
//! mirrors the same relative-path search `cli/tests/sandbox_e2e.rs` already
//! uses for this exact problem: check next to this test binary's own
//! `target/<profile>/` dir. Only built when the workspace (or at least the
//! `runtime` package) has been compiled — skip, don't fail, if it's absent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use horsie_executor::{
    ConnectedRuntimeRegistry, RuntimeEndpoint, RuntimeListenerServer, serve_runtime_connections,
};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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

#[tokio::test]
async fn connect_dials_and_registers_under_runtime_id() {
    let Some(runtime_bin) = locate_runtime_bin() else {
        eprintln!(
            "skipping connect_dials_and_registers_under_runtime_id: horsie-runtime \
             binary not found (run via `cargo test --workspace` to build it first)"
        );
        return;
    };

    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let listener =
        RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
            .await
            .expect("bind fake server");
    let addr = listener.tcp_addr().expect("tcp addr");
    let cancel = CancellationToken::new();
    serve_runtime_connections(listener, connected.clone(), cancel.clone());
    let _cancel_guard = cancel.drop_guard();

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

    let mut child = Command::new(env!("CARGO_BIN_EXE_horsie"))
        .args([
            "connect",
            "--server",
            &format!("http://{addr}"),
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--runtime-id",
            "test-runtime",
            "--config",
        ])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn horsie connect");

    let mut registered = false;
    for _ in 0..100 {
        if connected.runtime_transport("test-runtime").await.is_some() {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(registered, "test-runtime never registered within 2s");

    let _ = child.kill();
    let _ = child.wait();
}
