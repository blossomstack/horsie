//! Full provider↔runtime provision-step round trip: a real
//! `ProcessRuntimeProvider` spawns the real `horsie-runtime` binary, which
//! clones a local fixture repo via a `git_checkout` provision step before
//! announcing Ready.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use horsie_executor::{
    ConnectedRuntimeRegistry, ProcessRuntimeProvider, RuntimeEndpoint, RuntimeListenerServer,
    RuntimeProvider, serve_runtime_connections,
};
use horsie_models::executor::{ProvisionStep, RuntimeConfig, StepParam, WorkspaceConfig};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fixture_repo(dir: &Path) -> String {
    git(dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hello").unwrap();
    git(dir, &["add", "."]);
    git(
        dir,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "init",
        ],
    );
    format!("file://{}", dir.display())
}

fn checkout_step(url: &str, dir: &str) -> ProvisionStep {
    ProvisionStep {
        name: format!("checkout {dir}"),
        uses: "git_checkout".into(),
        with: vec![
            StepParam {
                key: "url".into(),
                value: url.into(),
            },
            StepParam {
                key: "dir".into(),
                value: dir.into(),
            },
        ],
    }
}

struct Assembly {
    provider: ProcessRuntimeProvider,
    _cancel_guard: tokio_util::sync::DropGuard,
}

async fn assembly(sock_dir: &Path) -> Assembly {
    let sock = sock_dir.join("rt.sock");
    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Unix(sock.clone()))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    serve_runtime_connections(listener, connected.clone(), cancel.clone());
    let provider = ProcessRuntimeProvider::new(
        PathBuf::from(env!("CARGO_BIN_EXE_horsie-runtime")),
        RuntimeEndpoint::Unix(sock),
        connected,
    );
    Assembly {
        provider,
        _cancel_guard: cancel.drop_guard(),
    }
}

fn config(ws: &Path, provision: Vec<ProvisionStep>) -> RuntimeConfig {
    RuntimeConfig {
        workspaces: vec![WorkspaceConfig {
            name: "main".into(),
            path: ws.to_string_lossy().into_owned(),
        }],
        plugins_dir: None,
        hook_path: vec![],
        env: vec![],
        provision,
    }
}

#[tokio::test]
async fn provision_steps_clone_before_ready() {
    let src = tempfile::tempdir().unwrap();
    let url = fixture_repo(src.path());
    let ws = tempfile::tempdir().unwrap();
    let sock = tempfile::tempdir().unwrap();
    let a = assembly(sock.path()).await;

    let handle = a
        .provider
        .create(
            "rt-prov-1",
            &config(ws.path(), vec![checkout_step(&url, "repo")]),
        )
        .await
        .expect("create with provision");
    // Ready arrived only after the clone completed.
    assert!(ws.path().join("repo/README.md").is_file());
    let _ = handle.stop().await;
}

#[tokio::test]
async fn provision_failure_reports_git_error() {
    let ws = tempfile::tempdir().unwrap();
    let sock = tempfile::tempdir().unwrap();
    let a = assembly(sock.path()).await;

    let result = a
        .provider
        .create(
            "rt-prov-2",
            &config(
                ws.path(),
                vec![checkout_step("file:///nonexistent-xyz", "repo")],
            ),
        )
        .await;
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("bad clone must fail create"),
    };
    let msg = err.to_string();
    assert!(msg.contains("git clone failed"), "got: {msg}");
}
