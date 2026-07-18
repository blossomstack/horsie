//! End-to-end tests for the session server: real axum HTTP + real event-sourced
//! actors + real FileJournal, driven over HTTP with reqwest. Only the sandbox
//! runtime (MockVendor) and the LLM (MockLlmServer) are doubled.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::{SinkExt, StreamExt};
use horsie_actor::{ActorRef, FileJournal, Journal, spawn_root};
use horsie_agentcore::LlmProvider;
use horsie_anthropic::AnthropicProvider;
use horsie_mock_llm::MockLlmServer;
use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
use horsie_models::runtime::{
    RuntimeInboundMessage, RuntimeOutboundMessage, RuntimeReady, ScanResponse,
    SessionStartResponse, ToolCallResponse, ToolOutput, ToolResult,
};
use horsie_server::config::{DbConfigStore, StoreDeps};
use horsie_server::http::{AppState, app};
use horsie_server::sessions::spec::ServerDeps;
use horsie_server::sessions::supervisor::{SessionSupervisor, SessionSupervisorCommand};
use horsie_server::vendor::RuntimeVendor;
use horsie_server::vendor::mock::MockVendor;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── harness ──────────────────────────────────────────────────────────────────

struct Server {
    addr: SocketAddr,
    supervisor: ActorRef<SessionSupervisorCommand>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Cleanly stop: drain the supervisor's live sessions, then abort the HTTP task.
    async fn shutdown(self) {
        let _ = self
            .supervisor
            .ask(|reply| SessionSupervisorCommand::Shutdown { reply })
            .await;
        self.task.abort();
    }
}

fn provider_at(url: &str) -> Arc<dyn LlmProvider> {
    Arc::new(
        AnthropicProvider::with_api_key("test-key")
            .unwrap()
            .with_base_url(url)
            .with_retry_delay_secs(0),
    )
}

fn block_caps() -> CapabilitySpec {
    CapabilitySpec {
        network: NetworkPolicy::Block(BlockNetwork {}),
        grants: vec![],
        unsafe_seatbelt_rules: None,
    }
}

/// Start a server incarnation on `journal_dir`, with `vendor` under name "mock"
/// and a single LLM provider "mock" pointing at `mock_url`.
async fn start_server(
    journal_dir: &Path,
    vendor: Arc<dyn RuntimeVendor>,
    mock_url: &str,
) -> Server {
    let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    providers.insert("mock".into(), provider_at(mock_url));
    let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
    vendors.insert("mock".into(), vendor);
    let deps = ServerDeps {
        provider_registry: Arc::new(std::sync::RwLock::new(providers)),
        vendors: Arc::new(std::sync::RwLock::new(vendors)),
        state_dir: journal_dir.join("state"),
        github_tokens: None,
        mcp: None,
        plugins: None,
    };
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(journal_dir.to_path_buf()));
    let (gtx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = spawn_root(SessionSupervisor::new(deps, gtx.clone()), journal.clone());
    // A real (empty) settings store backs `/api/config`; the session flow uses the
    // custom `mock` registry/vendor above, so the store's own registry is unused.
    let db = journal_dir.join("config.db");
    let opened = DbConfigStore::open(
        &format!("sqlite://{}", db.display()),
        StoreDeps {
            info: horsie_models::settings::ServerInfo {
                config_path: String::new(),
                database: String::new(),
                state_dir: String::new(),
                data_dir: String::new(),
                plugins_dir: String::new(),
                version: "test".into(),
            },
            local_runtime_listen: None,
        },
    )
    .await
    .unwrap();
    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(opened.pool.clone()),
        horsie_server::github::GithubApi::new(),
    ));
    let plugins = Arc::new(horsie_server::plugins::PluginService::new(
        horsie_server::plugins::PluginStore::new(opened.pool.clone()),
        horsie_server::plugins::ArtifactStore::new(journal_dir.join("plugin-artifacts")),
        b"e2e-secret".to_vec(),
    ));
    let mcp = Arc::new(horsie_server::mcp::McpService::new(
        horsie_server::mcp::McpStore::new(opened.pool.clone()),
        github.clone(),
    ));
    let state = AppState {
        supervisor: supervisor.clone(),
        journal,
        global_events: gtx,
        caps_finalize: Arc::new(|c| c),
        default_caps: block_caps(),
        plugins_dir: None,
        hook_path: vec![],
        config_store: opened.store,
        github,
        mcp,
        plugins,
        web_dir: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    // Give the accept loop a beat to come up.
    tokio::time::sleep(Duration::from_millis(50)).await;
    Server {
        addr,
        supervisor,
        task,
    }
}

async fn create_session(client: &reqwest::Client, addr: &SocketAddr) -> String {
    let body = serde_json::json!({
        "agent": { "model": "mock", "use_plugins": false },
        "workdirs": ["/tmp"],
        "vendor": "mock"
    });
    let res = client
        .post(format!("http://{addr}/api/sessions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let v: serde_json::Value = res.json().await.unwrap();
    v["session"]["id"].as_str().unwrap().to_string()
}

/// Like `create_session`, but selects a named vendor with no `workdirs`/
/// `repos` — the shape a shared-local-vendor session must use (it never
/// resolves a caller-supplied path).
async fn create_session_for_vendor(
    client: &reqwest::Client,
    addr: &SocketAddr,
    vendor: &str,
) -> String {
    let body = serde_json::json!({
        "agent": { "model": "mock", "use_plugins": false },
        "workdirs": [],
        "vendor": vendor
    });
    let res = client
        .post(format!("http://{addr}/api/sessions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let v: serde_json::Value = res.json().await.unwrap();
    v["session"]["id"].as_str().unwrap().to_string()
}

/// Start a server wired for the shared local runtime vendor: `DbConfigStore`
/// binds a real `local_runtime_listen` listener, and — unlike `start_server`,
/// whose `ServerDeps.vendors` is a hand-rolled map bypassing the store
/// entirely — `ServerDeps.vendors` is the SAME `SharedVendors` map
/// `DbConfigStore::open()` returns, so a daemon dialing in is visible to
/// session resolution exactly as it would be in production. Returns the
/// server handle plus the listener's bound address for dialing fake daemons
/// into.
async fn start_server_with_shared_local(journal_dir: &Path, mock_url: &str) -> (Server, SocketAddr) {
    let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    providers.insert("mock".into(), provider_at(mock_url));
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(journal_dir.to_path_buf()));
    let db = journal_dir.join("config.db");
    let opened = DbConfigStore::open(
        &format!("sqlite://{}", db.display()),
        StoreDeps {
            info: horsie_models::settings::ServerInfo {
                config_path: String::new(),
                database: String::new(),
                state_dir: String::new(),
                data_dir: String::new(),
                plugins_dir: String::new(),
                version: "test".into(),
            },
            local_runtime_listen: Some("127.0.0.1:0".to_string()),
        },
    )
    .await
    .unwrap();
    let local_addr = opened
        .store
        .local_daemon_listen_addr()
        .expect("shared local runtime vendor listener bound");
    let deps = ServerDeps {
        provider_registry: Arc::new(std::sync::RwLock::new(providers)),
        vendors: opened.vendors.clone(),
        state_dir: journal_dir.join("state"),
        github_tokens: None,
        mcp: None,
        plugins: None,
    };
    let (gtx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = spawn_root(SessionSupervisor::new(deps, gtx.clone()), journal.clone());
    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(opened.pool.clone()),
        horsie_server::github::GithubApi::new(),
    ));
    let plugins = Arc::new(horsie_server::plugins::PluginService::new(
        horsie_server::plugins::PluginStore::new(opened.pool.clone()),
        horsie_server::plugins::ArtifactStore::new(journal_dir.join("plugin-artifacts")),
        b"e2e-secret".to_vec(),
    ));
    let mcp = Arc::new(horsie_server::mcp::McpService::new(
        horsie_server::mcp::McpStore::new(opened.pool.clone()),
        github.clone(),
    ));
    let state = AppState {
        supervisor: supervisor.clone(),
        journal,
        global_events: gtx,
        caps_finalize: Arc::new(|c| c),
        default_caps: block_caps(),
        plugins_dir: None,
        hook_path: vec![],
        config_store: opened.store,
        github,
        mcp,
        plugins,
        web_dir: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (
        Server {
            addr,
            supervisor,
            task,
        },
        local_addr,
    )
}

/// A fake `horsie-runtime --endpoint ws://... --runtime-id <label>` daemon:
/// dials the shared local vendor's listener, announces Ready under `label`,
/// answers every tool call with a fixed stdout, and answers the workspace
/// scan every session provisioning always performs
/// (`session_actor.rs`'s `scan_workspace(...)` call, regardless of
/// `use_plugins`) with an empty result — otherwise the real `RuntimeClient`
/// awaits a `ScanResult` that never arrives and provisioning hangs forever.
fn spawn_fake_local_daemon(addr: SocketAddr, label: &str, reply: &str) -> JoinHandle<()> {
    let label = label.to_string();
    let reply = reply.to_string();
    tokio::spawn(async move {
        let (ws, _) = match connect_async(format!("ws://{addr}")).await {
            Ok(x) => x,
            Err(_) => return,
        };
        let (mut sink, mut stream) = ws.split();
        let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
            runtime_id: label,
            workdir: "/home/u/proj".to_string(),
        }))
        .unwrap();
        if sink.send(Message::Text(ready.into())).await.is_err() {
            return;
        }
        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else { continue };
            match serde_json::from_str::<RuntimeInboundMessage>(&text) {
                Ok(RuntimeInboundMessage::ToolCall(req)) => {
                    let resp = RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                        call_id: req.call_id,
                        result: ToolResult::Ok(ToolOutput {
                            stdout: reply.clone(),
                            stderr: String::new(),
                            exit_code: 0,
                        }),
                    });
                    if let Ok(json) = serde_json::to_string(&resp) {
                        let _ = sink.send(Message::Text(json.into())).await;
                    }
                }
                Ok(RuntimeInboundMessage::ScanWorkspace(req)) => {
                    let resp = RuntimeOutboundMessage::ScanResult(ScanResponse {
                        call_id: req.call_id,
                        workspaces: vec![],
                        shared_skills: vec![],
                    });
                    if let Ok(json) = serde_json::to_string(&resp) {
                        let _ = sink.send(Message::Text(json.into())).await;
                    }
                }
                Ok(RuntimeInboundMessage::SessionStart(req)) => {
                    let resp = RuntimeOutboundMessage::SessionStartResult(SessionStartResponse {
                        call_id: req.call_id,
                        context: String::new(),
                    });
                    if let Ok(json) = serde_json::to_string(&resp) {
                        let _ = sink.send(Message::Text(json.into())).await;
                    }
                }
                _ => {}
            }
        }
    })
}

async fn send_message(
    client: &reqwest::Client,
    addr: &SocketAddr,
    id: &str,
    text: &str,
) -> reqwest::StatusCode {
    client
        .post(format!("http://{addr}/api/sessions/{id}/messages"))
        .json(&serde_json::json!({ "text": text }))
        .send()
        .await
        .unwrap()
        .status()
}

async fn get_status(client: &reqwest::Client, addr: &SocketAddr, id: &str) -> Option<String> {
    let res = client
        .get(format!("http://{addr}/api/sessions/{id}"))
        .send()
        .await
        .unwrap();
    if res.status().as_u16() == 404 {
        return None;
    }
    let v: serde_json::Value = res.json().await.unwrap();
    Some(v["session"]["status"].as_str().unwrap().to_string())
}

/// Poll the session detail until its status equals `want` or the deadline passes.
async fn wait_status(client: &reqwest::Client, addr: &SocketAddr, id: &str, want: &str) {
    let deadline = Duration::from_secs(10);
    let start = std::time::Instant::now();
    loop {
        if let Some(s) = get_status(client, addr, id).await
            && s == want
        {
            return;
        }
        if start.elapsed() > deadline {
            let got = get_status(client, addr, id).await;
            panic!("timed out waiting for status {want}; last = {got:?}");
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

// ── SSE reader ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Ev {
    id: Option<u64>,
    kind: String,
    data: serde_json::Value,
}

/// Open an SSE stream and collect events until `stop` returns true or timeout.
async fn collect_sse(
    client: &reqwest::Client,
    url: &str,
    last_event_id: Option<u64>,
    stop: impl Fn(&[Ev]) -> bool,
) -> Vec<Ev> {
    use futures_util::StreamExt;
    let mut req = client.get(url).header("accept", "text/event-stream");
    if let Some(cursor) = last_event_id {
        req = req.header("last-event-id", cursor.to_string());
    }
    let resp = req.send().await.unwrap();
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut events: Vec<Ev> = Vec::new();

    let read = async {
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(_) => break,
            };
            buf.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(pos) = buf.find("\n\n") {
                let block: String = buf.drain(..pos + 2).collect();
                if let Some(ev) = parse_event(&block) {
                    events.push(ev);
                    if stop(&events) {
                        return;
                    }
                }
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(10), read).await;
    events
}

fn parse_event(block: &str) -> Option<Ev> {
    let mut id = None;
    let mut data = None;
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("id:") {
            id = rest.trim().parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data = Some(rest.trim().to_string());
        }
    }
    let data = data?;
    let json: serde_json::Value = serde_json::from_str(&data).ok()?;
    let kind = json.get("type")?.as_str()?.to_string();
    Some(Ev {
        id,
        kind,
        data: json,
    })
}

fn kinds(events: &[Ev]) -> Vec<String> {
    events.iter().map(|e| e.kind.clone()).collect()
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_message_sse_roundtrip() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("hello from the agent");
    let tmp = tempfile::tempdir().unwrap();
    let vendor = Arc::new(MockVendor::new());
    let server = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Connect SSE (replay from 0 + live) BEFORE sending, so we see the whole turn.
    let url = format!("http://{}/api/sessions/{id}/events", server.addr);
    let client2 = client.clone();
    let sse = tokio::spawn(async move {
        collect_sse(&client2, &url, None, |evs| {
            evs.iter().any(|e| e.kind == "TurnCompleted")
        })
        .await
    });
    // Small beat so the subscription is live before the turn runs.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        send_message(&client, &server.addr, &id, "hi")
            .await
            .as_u16(),
        202
    );

    let events = sse.await.unwrap();
    let ks = kinds(&events);
    assert!(ks.contains(&"Message".to_string()), "kinds: {ks:?}");
    assert!(ks.contains(&"TurnCompleted".to_string()), "kinds: {ks:?}");
    // The assistant's text made it through the stream (in a durable Message event).
    let joined = events
        .iter()
        .map(|e| e.data.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("hello from the agent"),
        "assistant text missing from stream: {joined}"
    );
    // Durable coarse events carry monotonic ids.
    let ids: Vec<u64> = events.iter().filter_map(|e| e.id).collect();
    assert!(!ids.is_empty());
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "ids not increasing: {ids:?}"
    );

    wait_status(&client, &server.addr, &id, "Idle").await;
    assert_eq!(vendor.signals(), vec![format!("create:{id}")]);

    server.shutdown().await;
}

#[tokio::test]
async fn stop_preserves_and_message_reattaches() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first");
    mock.queue_response("second");
    let tmp = tempfile::tempdir().unwrap();
    let vendor = Arc::new(MockVendor::new());
    let server = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Stop → runtime stopped but preserved.
    let res = client
        .post(format!("http://{}/api/sessions/{id}/stop", server.addr))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    wait_status(&client, &server.addr, &id, "Stopped").await;
    assert!(vendor.signals().contains(&format!("stop:{id}")));

    // A new message re-attaches and runs.
    assert_eq!(
        send_message(&client, &server.addr, &id, "two")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server.addr, &id, "Idle").await;
    assert!(vendor.signals().contains(&format!("attach:{id}")));

    server.shutdown().await;
}

#[tokio::test]
async fn restart_marks_interrupted_and_message_resumes() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let vendor = Arc::new(MockVendor::new());
    let client = reqwest::Client::new();

    let server = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // A blocking turn: the LLM request arrives, then hangs — the session is
    // Running when we simulate a crash.
    let block = mock.blocking_response("never delivered");
    send_message(&client, &server.addr, &id, "hang").await;
    block.wait_until_received().await;
    // Crash: stop the server core without letting the turn finish.
    server.shutdown().await;

    // New incarnation on the SAME journal recovers the registry and reconciles
    // the in-flight session to Interrupted — with no vendor calls.
    let signals_before = vendor.signals();
    let server2 = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    wait_status(&client, &server2.addr, &id, "Interrupted").await;
    assert_eq!(
        vendor.signals(),
        signals_before,
        "recovery must not emit vendor signals (lazy)"
    );

    // A new message attaches and completes the (now answerable) turn.
    mock.queue_response("resumed answer");
    assert_eq!(
        send_message(&client, &server2.addr, &id, "continue")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server2.addr, &id, "Idle").await;
    assert!(
        vendor
            .signals()
            .iter()
            .any(|s| s == &format!("attach:{id}"))
    );

    server2.shutdown().await;
}

#[tokio::test]
async fn attach_failure_lands_recovery_failed_then_retry_succeeds() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    // The first attach fails; the second succeeds.
    let vendor = Arc::new(MockVendor::new().fail_attach_times(1));
    let client = reqwest::Client::new();

    let server = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Stop, so the next message must attach.
    client
        .post(format!("http://{}/api/sessions/{id}/stop", server.addr))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    wait_status(&client, &server.addr, &id, "Stopped").await;

    // First message → attach fails → 502 + RecoveryFailed.
    let status = send_message(&client, &server.addr, &id, "one").await;
    assert_eq!(status.as_u16(), 502);
    wait_status(&client, &server.addr, &id, "RecoveryFailed").await;

    // Second message → attach succeeds → Idle.
    mock.queue_response("recovered");
    assert_eq!(
        send_message(&client, &server.addr, &id, "two")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server.addr, &id, "Idle").await;

    server.shutdown().await;
}

#[tokio::test]
async fn last_event_id_replay_is_gap_free() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("one");
    mock.queue_response("two");
    let tmp = tempfile::tempdir().unwrap();
    let vendor = Arc::new(MockVendor::new());
    let server = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "two").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Full replay from 0.
    let url = format!("http://{}/api/sessions/{id}/events", server.addr);
    let all = collect_sse(&client, &url, None, |evs| {
        evs.iter().filter(|e| e.kind == "TurnCompleted").count() >= 2
    })
    .await;
    let all_ids: Vec<u64> = all.iter().filter_map(|e| e.id).collect();
    assert!(all_ids.len() >= 2);
    let mid = all_ids[all_ids.len() / 2];

    // Reconnect after `mid`: only strictly-greater ids, no dupes, no gaps vs the
    // tail of the full replay.
    let after = collect_sse(&client, &url, Some(mid), |evs| {
        evs.iter().filter(|e| e.kind == "TurnCompleted").count() >= 1
    })
    .await;
    let after_ids: Vec<u64> = after.iter().filter_map(|e| e.id).collect();
    assert!(
        after_ids.iter().all(|i| *i > mid),
        "ids: {after_ids:?} mid {mid}"
    );
    let expected_tail: Vec<u64> = all_ids.iter().copied().filter(|i| *i > mid).collect();
    // The reconnect's stamped ids are a prefix of the full replay's tail.
    assert_eq!(
        &after_ids[..expected_tail.len().min(after_ids.len())],
        &expected_tail[..expected_tail.len().min(after_ids.len())]
    );

    server.shutdown().await;
}

#[tokio::test]
async fn repos_session_creates_and_reports_repos() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let vendor = Arc::new(MockVendor::new());
    let server = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "agent": {"model": "mock"},
        "workdirs": [],
        "vendor": "mock",
        "repos": [{"url": "https://github.com/o/api", "gitRef": "main"}]
    });
    let res = client
        .post(format!("http://{}/api/sessions", server.addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let created: serde_json::Value = res.json().await.unwrap();
    let id = created["session"]["id"].as_str().unwrap().to_string();

    // Provisioning runs the git_checkout step through the mock runtime and
    // lands the session Idle — a real (doubled) provisioning handshake, not
    // just a static echo of the request.
    wait_status(&client, &server.addr, &id, "Idle").await;

    let detail: serde_json::Value = client
        .get(format!("http://{}/api/sessions/{id}", server.addr))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        detail["session"]["repos"],
        serde_json::json!(["https://github.com/o/api"])
    );
    assert_eq!(detail["session"]["workdirs"], serde_json::json!([]));

    server.shutdown().await;
}

#[tokio::test]
async fn turn_in_flight_conflicts() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let vendor = Arc::new(MockVendor::new());
    let server = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Hang the first turn, then a concurrent message must 409.
    let block = mock.blocking_response("eventually");
    assert_eq!(
        send_message(&client, &server.addr, &id, "first")
            .await
            .as_u16(),
        202
    );
    block.wait_until_received().await;
    wait_status(&client, &server.addr, &id, "Running").await;

    let status = send_message(&client, &server.addr, &id, "second").await;
    assert_eq!(status.as_u16(), 409);

    block.release();
    wait_status(&client, &server.addr, &id, "Idle").await;

    server.shutdown().await;
}

/// End-to-end for the shared local runtime vendor's `open()` wiring: a real
/// `DbConfigStore` binds `local_runtime_listen`, a fake daemon dials in under
/// a label, and a session resolves that label through the SAME `SharedVendors`
/// map the store returns — the one seam a unit test of `LocalDaemonRegistry`
/// in isolation can't cover. Also exercises the vendor's disconnect →
/// `RecoveryFailed` → reconnect → resume cycle end to end over real HTTP.
#[tokio::test]
async fn shared_local_vendor_resolves_dialed_in_daemon_and_recovers_after_disconnect() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("hello from my-laptop");
    let tmp = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();

    let (server, local_addr) = start_server_with_shared_local(tmp.path(), &mock.url()).await;
    let daemon = spawn_fake_local_daemon(local_addr, "my-laptop", "shared-ok");
    // Give the dial-back handshake a beat to land before the session tries
    // to resolve the label.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let id = create_session_for_vendor(&client, &server.addr, "my-laptop").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    assert_eq!(
        send_message(&client, &server.addr, &id, "hi")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Stop, so the next message must re-attach — then disconnect the daemon
    // before that happens (simulating the user closing their laptop).
    client
        .post(format!("http://{}/api/sessions/{id}/stop", server.addr))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    wait_status(&client, &server.addr, &id, "Stopped").await;
    daemon.abort();
    // Give the abort a beat to actually drop the socket server-side.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Re-attach against a disconnected label fails → 502 + RecoveryFailed.
    let status = send_message(&client, &server.addr, &id, "are you still there").await;
    assert_eq!(status.as_u16(), 502);
    wait_status(&client, &server.addr, &id, "RecoveryFailed").await;

    // Reconnect a fresh daemon under the SAME label — the next message
    // resolves it via the same map and resumes normally.
    let daemon2 = spawn_fake_local_daemon(local_addr, "my-laptop", "shared-ok-again");
    tokio::time::sleep(Duration::from_millis(150)).await;
    mock.queue_response("resumed after reconnect");
    assert_eq!(
        send_message(&client, &server.addr, &id, "welcome back")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server.addr, &id, "Idle").await;

    daemon2.abort();
    server.shutdown().await;
}
