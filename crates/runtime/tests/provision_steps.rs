//! Provisioning is a request the runtime answers, not a phase it dies in.
//!
//! The old shape ran the steps before `Ready` and reported a failure by exiting
//! 5. That made three things impossible at once: nobody could time it, nobody
//! could retry it, and nobody could run it a second time — so a hibernated
//! runtime whose workspace did not survive could only be rebuilt from scratch.
//!
//! Against the real binary over a real socket. The idempotence test in
//! particular cannot be written against a mock: what it asserts is what `git`
//! does to a directory that already holds the checkout.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::{SinkExt, StreamExt};
use horsie_models::executor::{ProvisionStep, StepParam};
use horsie_models::runtime::{
    PingRequest, ProvisionResult, ProvisionWorkspaceRequest, RuntimeInboundMessage,
    RuntimeOutboundMessage,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Generous enough to cover a process spawn, a websocket upgrade and a local
/// clone; short enough that a hang fails the suite rather than stalling it.
const WINDOW: Duration = Duration::from_secs(30);

#[tokio::test]
async fn provision_workspace_clones_and_names_each_step_it_applied() {
    let fixture = tempfile::tempdir().unwrap();
    let url = fixture_repo(fixture.path());
    let ws = tempfile::tempdir().unwrap();
    let mut rt = Runtime::spawn(ws.path(), "rt-prov-ok").await;

    rt.provision("p1", vec![checkout_step(&url, "repo")]).await;

    match rt.next_provision_result("p1").await {
        ProvisionResult::Ok(ok) => assert_eq!(ok.applied, vec!["checkout repo".to_string()]),
        ProvisionResult::Err(e) => panic!("expected success, got {}", e.reason),
    }
    assert!(
        ws.path().join("repo/README.md").is_file(),
        "the step should have produced the checkout"
    );
}

/// The property the whole design rests on. The server sends this on every
/// acquisition rather than remembering whether it already did, because only the
/// runtime knows whether a hibernated workspace survived — so a second
/// identical request must be a cheap success, not an error and not a second
/// clone.
#[tokio::test]
async fn provisioning_the_same_steps_twice_is_a_success_both_times() {
    let fixture = tempfile::tempdir().unwrap();
    let url = fixture_repo(fixture.path());
    let ws = tempfile::tempdir().unwrap();
    let mut rt = Runtime::spawn(ws.path(), "rt-prov-twice").await;

    for call_id in ["p1", "p2"] {
        rt.provision(call_id, vec![checkout_step(&url, "repo")])
            .await;
        match rt.next_provision_result(call_id).await {
            ProvisionResult::Ok(_) => {}
            ProvisionResult::Err(e) => panic!("{call_id} failed: {}", e.reason),
        }
    }

    // One clone, not two nested inside each other.
    assert!(ws.path().join("repo/README.md").is_file());
    assert!(!ws.path().join("repo/repo").exists(), "cloned twice");
}

/// The failure this change exists to make visible. The old path wrote the git
/// error to the runtime's own stderr and exited 5, so the server saw a dead
/// process and the operator saw nothing at all.
#[tokio::test]
async fn a_bad_clone_reports_the_git_error_and_leaves_the_runtime_alive() {
    let ws = tempfile::tempdir().unwrap();
    let mut rt = Runtime::spawn(ws.path(), "rt-prov-bad").await;

    rt.provision("p1", vec![checkout_step("file:///nonexistent-xyz", "repo")])
        .await;

    match rt.next_provision_result("p1").await {
        ProvisionResult::Err(e) => assert!(
            e.reason.contains("git clone failed"),
            "the reason must carry git's own diagnosis, got: {}",
            e.reason
        ),
        ProvisionResult::Ok(_) => panic!("a clone of a nonexistent repository reported success"),
    }

    // Still serving. Under the old boot-phase path this process had exited.
    rt.send(RuntimeInboundMessage::Ping(PingRequest {
        call_id: "ping".to_string(),
    }))
    .await;
    let reply = rt.next_outbound().await;
    assert!(
        matches!(reply, RuntimeOutboundMessage::Pong(_)),
        "a failed provision must not kill the runtime, got {reply:?}"
    );
}

/// A runtime with a workspace and nothing else must reach `Ready` on its own.
/// It used to be that `Ready` also asserted provisioning had happened, which is
/// what made a silent failure indistinguishable from having nothing to do.
#[tokio::test]
async fn ready_arrives_without_anyone_asking_for_provisioning() {
    let ws = tempfile::tempdir().unwrap();
    let _rt = Runtime::spawn(ws.path(), "rt-prov-none").await;
    // `spawn` asserts the handshake; reaching here is the assertion.
}

// --- harness ---------------------------------------------------------------

struct Runtime {
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    child: tokio::process::Child,
}

impl Runtime {
    /// Spawn the real binary, accept its dial-back, and consume its handshake.
    async fn spawn(workspace: &Path, runtime_id: &str) -> Self {
        // Port 0: the OS picks, so parallel runs never collide.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let child =
            tokio::process::Command::new(PathBuf::from(env!("CARGO_BIN_EXE_horsie-runtime")))
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
        Self { ws, child }
    }

    async fn provision(&mut self, call_id: &str, steps: Vec<ProvisionStep>) {
        self.send(RuntimeInboundMessage::ProvisionWorkspace(
            ProvisionWorkspaceRequest {
                call_id: call_id.to_string(),
                steps,
            },
        ))
        .await;
    }

    async fn send(&mut self, message: RuntimeInboundMessage) {
        send(&mut self.ws, message).await;
    }

    async fn next_outbound(&mut self) -> RuntimeOutboundMessage {
        next_outbound(&mut self.ws).await
    }

    async fn next_provision_result(&mut self, call_id: &str) -> ProvisionResult {
        match self.next_outbound().await {
            RuntimeOutboundMessage::ProvisionResult(resp) => {
                assert_eq!(resp.call_id, call_id, "a reply answers its own request");
                resp.result
            }
            other => panic!("expected a ProvisionResult, got {other:?}"),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.child.start_kill().ok();
    }
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

/// The next decodable outbound message, skipping the frames the websocket layer
/// exchanges on its own.
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

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("running git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A one-commit repository, returned as a `file://` URL a clone can reach.
fn fixture_repo(dir: &Path) -> String {
    git(dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hello").unwrap();
    git(dir, &["add", "."]);
    git(
        dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
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
        uses: "git_checkout".to_string(),
        with: vec![
            StepParam {
                key: "url".to_string(),
                value: url.to_string(),
            },
            StepParam {
                key: "dir".to_string(),
                value: dir.to_string(),
            },
        ],
    }
}
