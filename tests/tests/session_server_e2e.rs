//! End-to-end tests for the session server: real axum HTTP + real event-sourced
//! actors + a real `SqlJournal`, driven over HTTP with reqwest. Only the sandbox
//! runtime (a FakeRuntimeVendor over a real WebSocket) and the LLM
//! (MockLlmServer) are doubled.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use horsie_actor::{ActorRef, Journal, spawn_root};
use horsie_agentcore::LlmProvider;
use horsie_anthropic::AnthropicProvider;
use horsie_mock_llm::MockLlmServer;
use horsie_runtime_vendor::ConnectedRuntimeRegistry;
use horsie_server::config::{DbConfigStore, StoreDeps};
use horsie_server::db::journal::SqlJournal;
use horsie_server::http::{AppState, app};
use horsie_server::runtime_manager::{RuntimeDeps, RuntimeManager};
use horsie_server::runtime_vendor::RuntimeVendorLink;
use horsie_server::runtime_vendor::fake::FakeRuntimeVendor;
use horsie_server::sessions::clock::TestClock;
use horsie_server::sessions::spec::ServerDeps;
use horsie_server::sessions::supervisor::{
    SessionSupervisor, SessionSupervisorCommand, SupervisorConfig,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The LLM messages on a `/history` page.
///
/// A page is a list of transcript *entries*, each a tagged union — a hook record
/// is an entry too, and is deliberately not a message. Tests that reason about
/// the conversation go through here so a new entry kind cannot silently change
/// what they count.
fn page_messages(page: &serde_json::Value) -> Vec<serde_json::Value> {
    page["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("a history page must carry entries: {page}"))
        .iter()
        .filter(|e| e["type"] == serde_json::json!("Llm"))
        .map(|e| e["value"].clone())
        .collect()
}

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

/// Start a server incarnation on `journal_dir`, with `vendor` under name "mock"
/// and a single LLM provider "mock" pointing at `mock_url`.
async fn start_server(
    journal_dir: &Path,
    vendor: Arc<RuntimeVendorLink>,
    mock_url: &str,
) -> Server {
    start_server_with(journal_dir, vendor, mock_url, None).await
}

/// As [`start_server`], but with the supervisor's idle policy under the test's
/// control: a clock that only moves when told, and no background ticker, so
/// offload happens exactly when the test sends `Tick` and never by surprise.
async fn start_server_with(
    journal_dir: &Path,
    vendor: Arc<RuntimeVendorLink>,
    mock_url: &str,
    clock: Option<Arc<TestClock>>,
) -> Server {
    let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    providers.insert("mock".into(), provider_at(mock_url));
    let mut vendors: HashMap<String, Arc<RuntimeVendorLink>> = HashMap::new();
    vendors.insert("mock".into(), vendor);
    let shared_vendors = Arc::new(std::sync::RwLock::new(vendors));
    let deps = ServerDeps {
        runtimes: Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors: shared_vendors.clone(),
            github_tokens: None,
            plugins: None,
        })),
        provider_registry: Arc::new(std::sync::RwLock::new(providers)),
        vendors: shared_vendors.clone(),
        github_tokens: None,
        mcp: None,
        plugins: None,
        memory: None,
    };
    // The e2e suite runs on the production default backend, so every test here
    // — including the restart ones — exercises real snapshots and compaction.
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
        },
        horsie_server::auth::UserId::new("1"),
    )
    .await
    .unwrap();
    let journal: Arc<dyn Journal> = Arc::new(SqlJournal::new(
        opened.db.clone(),
        horsie_server::auth::UserId::new("1"),
    ));
    let (gtx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = match clock {
        Some(clock) => spawn_root(
            SessionSupervisor::with_config(
                deps,
                gtx.clone(),
                SupervisorConfig {
                    clock,
                    idle_timeout: Duration::from_secs(180),
                    tick_interval: None,
                },
            ),
            journal.clone(),
        ),
        None => spawn_root(SessionSupervisor::new(deps, gtx.clone()), journal.clone()),
    };
    // A real (empty) settings store backs `/api/config`; the session flow uses
    // the custom `mock` registry/vendor above, so the store's own registry is
    // unused. It is the same store the journal above runs on.
    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        horsie_server::github::GithubApi::new(),
    ));
    let plugins = Arc::new(horsie_server::plugins::PluginService::new(
        horsie_server::plugins::PluginStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        horsie_server::plugins::MarketplaceStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        horsie_server::plugins::ArtifactStore::new(journal_dir.join("plugin-artifacts")),
        b"e2e-secret".to_vec(),
    ));
    let mcp = Arc::new(horsie_server::mcp::McpService::new(
        horsie_server::mcp::McpStore::new(opened.db.clone(), horsie_server::auth::UserId::new("1")),
        github.clone(),
    ));
    let memory = Arc::new(horsie_server::memory::MemoryService::new(
        horsie_server::memory::MemoryStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
    ));
    let agents = Arc::new(horsie_server::agents::AgentService::new(
        horsie_server::agents::AgentStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        opened.store.clone(),
    ));
    let routines = Arc::new(horsie_server::routines::RoutineService::new(
        horsie_server::routines::RoutineStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        agents.clone(),
    ));
    let environments = Arc::new(horsie_server::environments::EnvironmentService::new(
        horsie_server::environments::EnvironmentStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
    ));
    // Auth off: this suite drives the HTTP API without a credential, and a
    // disabled deployment is a supported configuration. Authenticated coverage
    // lives in the server crate's own HTTP tests.
    let auth = Arc::new(horsie_server::auth::AuthService::new(
        horsie_server::auth::AuthStore::new(opened.db.clone()),
        horsie_server::auth::AuthDeps {
            enabled: false,
            state_dir: journal_dir.to_path_buf(),
        },
    ));
    let vendor_agents = Arc::new(horsie_server::runtime_vendor::RuntimeVendorRegistry::new(
        shared_vendors,
    ));
    let workflows = Arc::new(horsie_server::workflows::WorkflowService::new(
        horsie_server::workflows::WorkflowStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        agents.clone(),
    ));
    let routine_runner = Arc::new(horsie_server::routines::RoutineRunner::new(
        routines.clone(),
        agents.clone(),
        opened.store.clone(),
        vendor_agents.clone(),
        supervisor.clone(),
    ));
    let state = AppState {
        supervisor: supervisor.clone(),
        global_events: gtx,
        auth,
        config_store: opened.store.clone(),
        model_cards: Arc::new(horsie_server::config::model_cards::ModelCardStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        )),
        chatgpt: Arc::new(
            horsie_server::config::chatgpt_login::ChatGptLoginService::new(
                opened.db.clone(),
                horsie_server::auth::UserId::new("1"),
                opened.store.clone(),
            ),
        ),
        github,
        mcp,
        plugins,
        memory,
        agents,
        routines,
        workflows,
        routine_runner,
        environments,
        vendor_agents,
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

/// Like `create_session`, but selects a named vendor with no `repos` — the
/// shape a shared-local-vendor session must use (it provisions nothing).
async fn create_session_for_vendor(
    client: &reqwest::Client,
    addr: &SocketAddr,
    vendor: &str,
) -> String {
    let body = serde_json::json!({
        "agent": { "model": "mock", "use_plugins": false },
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

/// Start a server whose `ServerDeps.vendors` is the SAME `SharedVendors` map
/// `DbConfigStore::open()` returns — unlike `start_server`, which builds its own.
/// That is the seam a vendor agent needs: an agent dialing
/// `/api/vendor/connect` publishes itself into that map and becomes resolvable
/// by session creation exactly as in production. Returns the server handle plus
/// the HTTP address agents dial.
async fn start_server_with_live_vendors(
    journal_dir: &Path,
    mock_url: &str,
) -> (Server, SocketAddr) {
    let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    providers.insert("mock".into(), provider_at(mock_url));
    let db = journal_dir.join("config.db");
    let _runtime_registry = Arc::new(ConnectedRuntimeRegistry::new());
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
        },
        horsie_server::auth::UserId::new("1"),
    )
    .await
    .unwrap();
    let deps = ServerDeps {
        runtimes: Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors: opened.vendors.clone(),
            github_tokens: None,
            plugins: None,
        })),
        provider_registry: Arc::new(std::sync::RwLock::new(providers)),
        vendors: opened.vendors.clone(),
        github_tokens: None,
        mcp: None,
        plugins: None,
        memory: None,
    };
    let journal: Arc<dyn Journal> = Arc::new(SqlJournal::new(
        opened.db.clone(),
        horsie_server::auth::UserId::new("1"),
    ));
    let (gtx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = spawn_root(SessionSupervisor::new(deps, gtx.clone()), journal.clone());
    let github = Arc::new(horsie_server::github::GithubService::new(
        horsie_server::github::GithubStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        horsie_server::github::GithubApi::new(),
    ));
    let plugins = Arc::new(horsie_server::plugins::PluginService::new(
        horsie_server::plugins::PluginStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        horsie_server::plugins::MarketplaceStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        horsie_server::plugins::ArtifactStore::new(journal_dir.join("plugin-artifacts")),
        b"e2e-secret".to_vec(),
    ));
    let mcp = Arc::new(horsie_server::mcp::McpService::new(
        horsie_server::mcp::McpStore::new(opened.db.clone(), horsie_server::auth::UserId::new("1")),
        github.clone(),
    ));
    let memory = Arc::new(horsie_server::memory::MemoryService::new(
        horsie_server::memory::MemoryStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
    ));
    let agents = Arc::new(horsie_server::agents::AgentService::new(
        horsie_server::agents::AgentStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        opened.store.clone(),
    ));
    let routines = Arc::new(horsie_server::routines::RoutineService::new(
        horsie_server::routines::RoutineStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        agents.clone(),
    ));
    let environments = Arc::new(horsie_server::environments::EnvironmentService::new(
        horsie_server::environments::EnvironmentStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
    ));
    // Auth off: this suite drives the HTTP API without a credential, and a
    // disabled deployment is a supported configuration. Authenticated coverage
    // lives in the server crate's own HTTP tests.
    let auth = Arc::new(horsie_server::auth::AuthService::new(
        horsie_server::auth::AuthStore::new(opened.db.clone()),
        horsie_server::auth::AuthDeps {
            enabled: false,
            state_dir: journal_dir.to_path_buf(),
        },
    ));
    let vendor_agents = Arc::new(horsie_server::runtime_vendor::RuntimeVendorRegistry::new(
        opened.vendors.clone(),
    ));
    let workflows = Arc::new(horsie_server::workflows::WorkflowService::new(
        horsie_server::workflows::WorkflowStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        ),
        agents.clone(),
    ));
    let routine_runner = Arc::new(horsie_server::routines::RoutineRunner::new(
        routines.clone(),
        agents.clone(),
        opened.store.clone(),
        vendor_agents.clone(),
        supervisor.clone(),
    ));
    let state = AppState {
        supervisor: supervisor.clone(),
        global_events: gtx,
        auth,
        config_store: opened.store.clone(),
        model_cards: Arc::new(horsie_server::config::model_cards::ModelCardStore::new(
            opened.db.clone(),
            horsie_server::auth::UserId::new("1"),
        )),
        chatgpt: Arc::new(
            horsie_server::config::chatgpt_login::ChatGptLoginService::new(
                opened.db.clone(),
                horsie_server::auth::UserId::new("1"),
                opened.store.clone(),
            ),
        ),
        github,
        mcp,
        plugins,
        memory,
        agents,
        routines,
        workflows,
        routine_runner,
        environments,
        vendor_agents,
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
        addr,
    )
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

async fn get_detail(client: &reqwest::Client, addr: &SocketAddr, id: &str) -> serde_json::Value {
    client
        .get(format!("http://{addr}/api/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Poll the detail endpoint until the inbox holds exactly `want` texts.
async fn wait_inbox(client: &reqwest::Client, addr: &SocketAddr, id: &str, want: &[&str]) {
    let deadline = Duration::from_secs(10);
    let start = std::time::Instant::now();
    loop {
        let detail = get_detail(client, addr, id).await;
        let got: Vec<String> = detail["session"]["inbox"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["text"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if got == want {
            return;
        }
        if start.elapsed() > deadline {
            panic!("timed out waiting for inbox {want:?}; last = {got:?}");
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
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
    // `null` only if the session's actor could not answer; reading a session
    // loads it, and a loaded session always has a status.
    Some(
        v["session"]["status"]
            .as_str()
            .unwrap_or("Unknown")
            .to_string(),
    )
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
    id: Option<String>,
    kind: String,
    data: serde_json::Value,
}

/// Open an SSE stream and collect events until `stop` returns true or timeout.
async fn collect_sse(
    client: &reqwest::Client,
    url: &str,
    last_event_id: Option<&str>,
    stop: impl Fn(&[Ev]) -> bool,
) -> Vec<Ev> {
    use futures_util::StreamExt;
    let mut req = client.get(url).header("accept", "text/event-stream");
    if let Some(cursor) = last_event_id {
        req = req.header("last-event-id", cursor);
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
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                id = Some(trimmed.to_string());
            }
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
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Connect SSE (replay from 0 + live) BEFORE sending, so we see the whole turn.
    let url = format!(
        "http://{}/api/sessions/{id}/agents/main/events",
        server.addr
    );
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
    assert!(ks.contains(&"Appended".to_string()), "kinds: {ks:?}");
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
    // Only appends carry an SSE id, and it is the message id — the same cursor
    // `/history` pages with, so a client has one vocabulary for both.
    let ids: Vec<String> = events.iter().filter_map(|e| e.id.clone()).collect();
    assert!(!ids.is_empty());
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "ids must be unique: {ids:?}");
    for ev in &events {
        if ev.id.is_some() {
            assert_eq!(ev.kind, "Appended", "only appends may carry an id");
        }
    }

    wait_status(&client, &server.addr, &id, "Idle").await;
    assert_eq!(
        agent.signals(),
        vec![
            format!("create:{id}"),
            format!("get:{id}"),
            format!("get:{id}")
        ],
        "one create at session creation, then two gets for the first turn: the \
         pre-run hook seam needs a runtime before the turn snapshots its \
         history, and `provide` still resolves one of its own so a hibernated \
         runtime is resumed on every run. Later turns reuse the cached handle \
         and cost one apiece. `get` never provisions."
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_queued_message_is_visible_on_the_detail_endpoint_and_the_stream() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // A turn that hangs inside the provider call, so the session is genuinely
    // Running when the second message arrives.
    let block = mock.blocking_response("first");
    send_message(&client, &server.addr, &id, "one").await;
    block.wait_until_received().await;

    // Subscribe while the turn is in flight — this stands in for a second tab,
    // which must learn about the queue without reloading the page. The inbox is
    // session-scoped, so it rides the session stream, not an agent's.
    let url = format!("http://{}/api/sessions/{id}/events", server.addr);
    let client2 = client.clone();
    let sse = tokio::spawn(async move {
        collect_sse(&client2, &url, None, |evs| {
            evs.iter().any(|e| e.kind == "InboxChanged")
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        send_message(&client, &server.addr, &id, "two")
            .await
            .as_u16(),
        202,
        "a message sent during a run is accepted, not refused"
    );

    // The detail endpoint is the durable source of the queue.
    let detail = get_detail(&client, &server.addr, &id).await;
    let inbox = detail["session"]["inbox"].as_array().unwrap();
    assert_eq!(inbox.len(), 1, "{detail}");
    assert_eq!(inbox[0]["text"], "two");
    assert!(
        inbox[0]["id"].as_str().is_some_and(|s| !s.is_empty()),
        "a queued message carries the id the send acknowledged: {detail}"
    );

    // ... and the live stream says the same thing.
    let events = sse.await.unwrap();
    let queued = events
        .iter()
        .find(|e| e.kind == "InboxChanged")
        .unwrap_or_else(|| panic!("no InboxChanged frame: {:?}", kinds(&events)));
    let q = queued.data["value"]["queued"].as_array().unwrap();
    assert_eq!(q.len(), 1, "{}", queued.data);
    assert_eq!(q[0]["text"], "two");

    // Letting the turn finish carries the message out of the queue.
    mock.queue_response("second");
    block.release();
    wait_inbox(&client, &server.addr, &id, &[]).await;

    server.shutdown().await;
}

#[tokio::test]
async fn prep_progressions_stream_during_a_turn() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("done");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Subscribe before sending so the live progression frames are seen. Prep
    // is session-scoped, so it streams on the session — and the turn's end is
    // observed there as the status returning to Idle.
    let url = format!("http://{}/api/sessions/{id}/events", server.addr);
    let client2 = client.clone();
    let sse = tokio::spawn(async move {
        collect_sse(&client2, &url, None, |evs| {
            evs.iter()
                .any(|e| e.kind == "StatusChanged" && e.data["value"]["status"] == "Idle")
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    send_message(&client, &server.addr, &id, "hi").await;

    let events = sse.await.unwrap();
    // Preparation stages surface as `Progressed` events before the reply.
    let stages: Vec<String> = events
        .iter()
        .filter(|e| e.kind == "Progressed")
        .filter_map(|e| e.data["value"]["stage"].as_str().map(str::to_string))
        .collect();
    assert!(
        stages.iter().any(|s| s == "scanning_workspace"),
        "missing scanning_workspace progression: {stages:?}"
    );
    assert!(
        stages.iter().any(|s| s == "ready"),
        "missing ready progression: {stages:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn history_endpoint_returns_windowed_messages() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first reply");
    mock.queue_response("second reply");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Two completed turns → user + assistant per turn = 4 messages. Poll the
    // history until both turns have landed (status can read `Idle` between the
    // 202 and the turn flipping to `Running`, so a bare `wait_status` races).
    let history = |limit: u32, before: Option<String>| {
        let client = client.clone();
        let addr = server.addr;
        let id = id.clone();
        async move {
            let mut url =
                format!("http://{addr}/api/sessions/{id}/agents/main/history?limit={limit}");
            if let Some(b) = before {
                url.push_str(&format!("&before={b}"));
            }
            client
                .get(url)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let history_after = |limit: usize, after: &str| {
        let client = client.clone();
        let addr = server.addr;
        let id = id.clone();
        let after = after.to_string();
        async move {
            client
                .get(format!(
                    "http://{addr}/api/sessions/{id}/agents/main/history?limit={limit}&after={after}"
                ))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let wait_for = |want: usize| {
        let history = &history;
        async move {
            let mut waited = 0;
            loop {
                let all = history(100, None).await;
                if page_messages(&all).len() >= want {
                    break;
                }
                assert!(waited < 100, "history never reached {want} messages");
                tokio::time::sleep(Duration::from_millis(50)).await;
                waited += 1;
            }
        }
    };
    // Serialize the turns: a second send while the first is Running would be
    // queued and merged into the next turn,
    // so wait for turn one's reply (2 messages) before sending turn two.
    send_message(&client, &server.addr, &id, "one").await;
    wait_for(2).await;
    send_message(&client, &server.addr, &id, "two").await;
    wait_for(4).await;

    // Tail page with a small limit: newest messages, older ones still owed.
    let page = history(2, None).await;
    let msgs = page_messages(&page);
    assert_eq!(msgs.len(), 2, "tail limit not honored: {page}");
    assert_eq!(page["hasMoreBefore"], serde_json::json!(true));
    assert_eq!(
        page["hasMoreAfter"],
        serde_json::json!(false),
        "the tail is the newest window: {page}"
    );
    // The newest assistant reply is in the tail window.
    let joined = page.to_string();
    assert!(
        joined.contains("second reply"),
        "tail missing latest: {page}"
    );

    // Every message the endpoint serves is stamped, and a turn's messages run
    // forward in time — the property a duration readout or a stuck-turn
    // watchdog is built on.
    let all = history(100, None).await;
    let stamps: Vec<u64> = page_messages(&all)
        .iter()
        .map(|m| {
            m["createdAtMs"]
                .as_u64()
                .unwrap_or_else(|| panic!("message without a stamp: {m}"))
        })
        .collect();
    assert!(
        stamps.iter().all(|&t| t > 1_700_000_000_000),
        "stamps must be real epoch millis, got {stamps:?}"
    );
    assert!(
        stamps.windows(2).all(|w| w[0] <= w[1]),
        "history must run forward in time, got {stamps:?}"
    );
    let assistant = page_messages(&all)
        .into_iter()
        .find(|m| m["role"] == serde_json::json!("Assistant"))
        .expect("an assistant reply");
    let started = assistant["startedAtMs"]
        .as_u64()
        .expect("an assistant message reports when its provider call began");
    assert!(
        started <= assistant["createdAtMs"].as_u64().unwrap(),
        "generation cannot end before it began: {assistant}"
    );

    // Scroll back before the oldest returned id → older messages.
    let oldest_id = msgs[0]["id"].as_str().unwrap().to_string();
    let older = history(2, Some(oldest_id.clone())).await;
    assert_eq!(page_messages(&older).len(), 2);
    assert_eq!(older["hasMoreBefore"], serde_json::json!(false));
    assert_eq!(
        older["hasMoreAfter"],
        serde_json::json!(true),
        "newer messages follow a scroll-back window: {older}"
    );

    // Forward from that same id → the newer half, which is what a reconnecting
    // stream backfills with. The two cursors are one space read both ways.
    let newer = history_after(2, &oldest_id).await;
    let newer_msgs = page_messages(&newer);
    let newer_ids: Vec<&str> = newer_msgs
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(
        !newer_ids.contains(&oldest_id.as_str()),
        "an `after` page must exclude the cursor itself: {newer}"
    );
    // `oldest_id` is the second-newest of four, so exactly one follows it.
    assert_eq!(newer_ids.len(), 1, "{newer}");
    assert_eq!(
        newer_ids[0],
        msgs[1]["id"].as_str().unwrap(),
        "forward paging must resume at the very next message: {newer}"
    );
    assert_eq!(newer["hasMoreAfter"], serde_json::json!(false));

    server.shutdown().await;
}

#[tokio::test]
async fn usage_endpoint_aggregates_across_turns_and_survives_restart() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first reply");
    mock.queue_response("second reply");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let client = reqwest::Client::new();

    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    let usage = |addr: SocketAddr, id: String| {
        let client = client.clone();
        async move {
            client
                .get(format!("http://{addr}/api/sessions/{id}/agents/main"))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };

    // Fresh session: zeroed usage, no completed turns yet. Session-wide total
    // is on the session document; the agent's own is on the agent document.
    let zero = usage(server.addr, id.clone()).await;
    assert_eq!(zero["agent"]["usage"]["inputTokens"], 0);
    let zero_detail = get_detail(&client, &server.addr, &id).await;
    assert_eq!(zero_detail["session"]["usageTotal"]["inputTokens"], 0);

    // Turn one, then poll until its usage has landed (the 202 races the
    // actual completion).
    send_message(&client, &server.addr, &id, "one").await;
    let after_one = loop {
        let v = get_detail(&client, &server.addr, &id).await;
        if v["session"]["usageTotal"]["inputTokens"].as_u64().unwrap() > 0 {
            break v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    wait_status(&client, &server.addr, &id, "Idle").await;
    let after_one_input = after_one["session"]["usageTotal"]["inputTokens"]
        .as_u64()
        .unwrap();

    // Turn two accumulates on top of turn one — the session-level total and
    // the (only) agent's own total must agree, since there's just one agent.
    send_message(&client, &server.addr, &id, "two").await;
    let after_two = loop {
        let v = get_detail(&client, &server.addr, &id).await;
        let total = v["session"]["usageTotal"]["inputTokens"].as_u64().unwrap();
        if total > after_one_input {
            break v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    wait_status(&client, &server.addr, &id, "Idle").await;
    let agent_doc = usage(server.addr, id.clone()).await;
    assert_eq!(
        after_two["session"]["usageTotal"], agent_doc["agent"]["usage"],
        "one agent: the session total must equal its own total: {after_two}"
    );

    // Crash + restart on the same journal, with no message sent yet on the new
    // incarnation: the session-level total must already be durable (it was
    // pushed and journaled by SessionActor as each turn completed), readable
    // with zero agent journal replay -- the new incarnation's agent hasn't
    // even been asked anything yet at this point.
    server.shutdown().await;
    let server2 = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let after_restart = get_detail(&client, &server2.addr, &id).await;
    assert_eq!(
        after_restart["session"]["usageTotal"], after_two["session"]["usageTotal"],
        "session-level usage total must survive a restart unchanged: {after_restart}"
    );

    server2.shutdown().await;
}

/// Compaction is real now, and a pause is where it happens: the snapshot on
/// cancel deletes every event it folded in. That is only safe if recovery
/// actually reads the snapshot — so this drives turns, forces a compaction, and
/// restarts on the same database to prove nothing was lost with the events.
#[tokio::test]
async fn a_compacted_session_recovers_its_whole_transcript_after_a_restart() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Stop only cancels a turn that is actually running, and cancelling is what
    // snapshots and compacts — so block the second turn mid-flight rather than
    // letting it finish, or this test would prove nothing.
    let block = mock.blocking_response("second");
    send_message(&client, &server.addr, &id, "two").await;
    block.wait_until_received().await;

    let res = client
        .post(format!("http://{}/api/sessions/{id}/stop", server.addr))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    block.release();
    wait_status(&client, &server.addr, &id, "Idle").await;

    let history = |addr: std::net::SocketAddr| {
        let client = client.clone();
        let id = id.clone();
        async move {
            client
                .get(format!(
                    "http://{addr}/api/sessions/{id}/agents/main/history?limit=100"
                ))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let before = history(server.addr).await;
    let before_ids: Vec<String> = page_messages(&before)
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        before_ids.len() >= 3,
        "a completed turn plus the cancelled turn's user message: {before}"
    );

    // Restart on the same database. Recovery must come from the snapshot plus
    // whatever events survived compaction — a full replay is no longer possible,
    // because those events are gone.
    server.shutdown().await;
    let server2 = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let after = history(server2.addr).await;
    let after_ids: Vec<String> = page_messages(&after)
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        after_ids, before_ids,
        "a compacted session must recover exactly the transcript it had: {after}"
    );

    server2.shutdown().await;
}

#[tokio::test]
async fn stop_cancels_the_turn_and_a_later_message_runs_again() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first");
    mock.queue_response("second");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Stop cancels the turn and nothing else: the runtime is the supervisor's
    // to release when the session goes cold, not the user's to destroy.
    let res = client
        .post(format!("http://{}/api/sessions/{id}/stop", server.addr))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    wait_status(&client, &server.addr, &id, "Idle").await;
    assert!(
        !agent.signals().contains(&format!("hibernate:{id}")),
        "stop must not hibernate: {:?}",
        agent.signals()
    );

    // A new message runs against the same runtime.
    assert_eq!(
        send_message(&client, &server.addr, &id, "two")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server.addr, &id, "Idle").await;
    assert!(agent.signals().contains(&format!("get:{id}")));

    server.shutdown().await;
}

#[tokio::test]
async fn restart_reconciles_the_interrupted_turn_and_never_resumes() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let client = reqwest::Client::new();

    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // A blocking turn: the LLM request arrives, then hangs — the session is
    // Running when we simulate a crash.
    let block = mock.blocking_response("never delivered");
    send_message(&client, &server.addr, &id, "hang").await;
    block.wait_until_received().await;
    // Crash: stop the server core without letting the turn finish.
    server.shutdown().await;

    // New incarnation on the SAME journal. Reading the session loads it, which
    // reconciles the turn the old process died in — it does not resume it, and
    // calls no vendor.
    let signals_before = agent.signals();
    let server2 = start_server(tmp.path(), agent.link(), &mock.url()).await;
    wait_status(&client, &server2.addr, &id, "Idle").await;
    assert_eq!(
        agent.signals(),
        signals_before,
        "recovery must not emit vendor signals (lazy)"
    );

    // A message then runs a fresh turn on the repaired history.
    mock.queue_response("resumed answer");
    assert_eq!(
        send_message(&client, &server2.addr, &id, "continue")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server2.addr, &id, "Idle").await;
    assert!(agent.signals().iter().any(|s| s == &format!("get:{id}")));

    server2.shutdown().await;
}

#[tokio::test]
async fn last_event_id_replay_is_gap_free() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("one");
    mock.queue_response("two");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "two").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    let url = format!(
        "http://{}/api/sessions/{id}/agents/main/events",
        server.addr
    );

    // A connect with no cursor is live-only — it does not replay, because a
    // stream is not a log. So drive a third turn with the stream open to learn
    // this transcript's ids.
    let live_url = url.clone();
    let live_client = client.clone();
    let live = tokio::spawn(async move {
        collect_sse(&live_client, &live_url, None, |evs| {
            evs.iter().filter(|e| e.kind == "TurnCompleted").count() >= 1
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    mock.queue_response("three");
    send_message(&client, &server.addr, &id, "three").await;
    let streamed = live.await.unwrap();

    let all_ids: Vec<String> = streamed.iter().filter_map(|e| e.id.clone()).collect();
    assert!(
        all_ids.len() >= 2,
        "the turn must append at least a user and an assistant message: {all_ids:?}"
    );
    let mid = all_ids[all_ids.len() / 2].clone();

    // Reconnect after `mid`. The backfill is served from the agent's state, and
    // must be exactly the tail of what the live stream delivered — no gap, no
    // duplicate, no dependence on any journal position.
    let expected_tail: Vec<String> = all_ids
        .iter()
        .skip_while(|i| **i != mid)
        .skip(1)
        .cloned()
        .collect();
    let after = collect_sse(&client, &url, Some(&mid), |evs| {
        evs.iter().filter_map(|e| e.id.clone()).count() >= expected_tail.len()
    })
    .await;
    let after_ids: Vec<String> = after.iter().filter_map(|e| e.id.clone()).collect();
    assert_eq!(
        after_ids, expected_tail,
        "reconnect must resume exactly after the cursor"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn repos_session_creates_and_reports_repos() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "agent": {"model": "mock"},
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

    server.shutdown().await;
}

#[tokio::test]
async fn session_detail_echoes_full_config() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "agent": {"model": "mock", "usePlugins": true, "mcpServers": ["gh"]},
        "vendor": "mock"
    });
    let res = client
        .post(format!("http://{}/api/sessions", server.addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let id = res.json::<serde_json::Value>().await.unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let detail: serde_json::Value = client
        .get(format!("http://{}/api/sessions/{id}", server.addr))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["session"]["usePlugins"], serde_json::json!(true));
    assert_eq!(detail["session"]["mcpServers"], serde_json::json!(["gh"]));
    assert_eq!(detail["session"]["plugins"], serde_json::json!([]));

    server.shutdown().await;
}

#[tokio::test]
async fn session_detail_echoes_thinking_effort() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    // The mock model is not in the settings store by default, and the server
    // only accepts a session thinking effort that the model offers — so
    // configure it through the same PUT /api/config path settings uses.
    let res = client
        .put(format!("http://{}/api/config", server.addr))
        .json(&serde_json::json!({
            "providers": [{
                "name": "mock",
                "kind": "anthropic",
                "baseUrl": mock.url(),
                "apiKey": "test-key"
            }],
            "models": [{
                "alias": "mock",
                "provider": "mock",
                "modelId": "mock-model",
                "thinkingEfforts": ["low", "high"],
                "thinkingEffort": "high"
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);

    // An explicit choice rides through to the detail endpoint...
    let body = serde_json::json!({
        "agent": {"model": "mock", "thinkingEffort": "low"},
        "vendor": "mock"
    });
    let res = client
        .post(format!("http://{}/api/sessions", server.addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let id = res.json::<serde_json::Value>().await.unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let detail: serde_json::Value = client
        .get(format!("http://{}/api/sessions/{id}", server.addr))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        detail["session"]["thinkingEffort"],
        serde_json::json!("low"),
        "explicit choice must appear on the session detail"
    );

    // ...and an omitted choice freezes the model's configured default.
    let body = serde_json::json!({
        "agent": {"model": "mock"},
        "vendor": "mock"
    });
    let res = client
        .post(format!("http://{}/api/sessions", server.addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let id = res.json::<serde_json::Value>().await.unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let detail: serde_json::Value = client
        .get(format!("http://{}/api/sessions/{id}", server.addr))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        detail["session"]["thinkingEffort"],
        serde_json::json!("high"),
        "model default must be frozen onto the session detail"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_dead_agent_link_fails_the_next_turn_visibly_instead_of_hanging() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mock = MockLlmServer::builder().build().await;
        let tmp = tempfile::tempdir().unwrap();
        // The agent hangs up on its first tool call, taking the link with it.
        let agent = FakeRuntimeVendor::builder("mock")
            .disconnect_after_tool_calls(0)
            .serve_in_process()
            .await
            .expect("fake agent");
        let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
        let client = reqwest::Client::new();
        let id = create_session(&client, &server.addr).await;
        wait_status(&client, &server.addr, &id, "Idle").await;

        mock.queue_tool_call("bash", serde_json::json!({ "command": "echo hi" }));
        mock.queue_response("done anyway");
        send_message(&client, &server.addr, &id, "first").await;

        // The turn must reach a terminal state. Which one depends on where the
        // hangup lands, but "still Running forever" is the failure this guards:
        // #61 item 2 was a session pinning a transport that could never answer.
        let mut settled = None;
        for _ in 0..200 {
            match get_status(&client, &server.addr, &id).await.as_deref() {
                Some("Running") | None => {}
                Some(other) => {
                    settled = Some(other.to_string());
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let settled = settled.expect("the turn never left Running after the agent hung up");
        assert!(
            matches!(settled.as_str(), "Idle" | "Failed" | "Unrecoverable"),
            "expected a terminal status, got {settled}"
        );

        server.shutdown().await;
    })
    .await
    .expect("test timed out");
}

/// #61 item 23: tool-call cancellation is never propagated to the sandbox.
///
/// On cancel, `Agent::run` drops the in-flight tool futures
/// (`agentcore/src/agent.rs:574-578`), which abandons them locally only.
/// `RuntimeClient::cancel(call_id)` exists, the transport declares it, and the
/// executor WS protocol implements `CancelToolCall` — but a repo-wide grep finds
/// no caller outside the executor's own inbound handler. Stopping a turn
/// mid-`bash` leaves the command running to completion inside the sandbox,
/// holding resources, with its output discarded.
///
/// This used to hang: `SessionActor::cancel_run` asked `RuntimeManager` for a
/// *fresh* client (`GetRuntime` on the vendor link) before cancelling, and that
/// call sat on the session's own mailbox. The fake agent's command loop is
/// sequential, so while it is inside `gate.wait()` answering the blocked tool
/// call it cannot read the `GetRuntime` either — the mailbox that `POST /stop`
/// needs never came free. The fix (`session_actor.rs`'s `cancel_run`) cancels
/// through the client the run already acquired in `provide()` instead: no
/// vendor round-trip, just a one-way `CancelCall` write that the fake picks up
/// once the tool call it targets releases the read loop.
#[tokio::test]
async fn stopping_a_turn_cancels_the_in_flight_tool_call() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mock = MockLlmServer::builder().build().await;
        let tmp = tempfile::tempdir().unwrap();

        // The agent holds every tool call, so Stop lands while one is genuinely
        // in flight, and records the cancels it receives.
        let agent = FakeRuntimeVendor::builder("mock")
            .block_tool_calls()
            .serve_in_process()
            .await
            .expect("fake agent");
        let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
        let client = reqwest::Client::new();
        let id = create_session(&client, &server.addr).await;
        wait_status(&client, &server.addr, &id, "Idle").await;

        mock.queue_tool_call("bash", serde_json::json!({ "command": "sleep 999" }));
        send_message(&client, &server.addr, &id, "run something slow").await;
        wait_status(&client, &server.addr, &id, "Running").await;

        // `Running` is reported at turn *start* — before the provider answers and
        // before any tool call reaches the runtime. Stopping there cancels an empty
        // in-flight set and writes nothing to the wire, so the assertion below would
        // be measuring a race rather than the behaviour. Wait for the call to
        // genuinely arrive: the fake records it before blocking on its gate, and
        // `RuntimeClient` tracks a call before sending it, so an arrival here means
        // `cancel_in_flight` has something to find.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while agent.tool_agent_ids().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the tool call never reached the runtime, so there was nothing to \
                 cancel (signals seen: {:?})",
                agent.signals()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let res = client
            .post(format!("http://{}/api/sessions/{id}/stop", server.addr))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 200);
        wait_status(&client, &server.addr, &id, "Idle").await;
        agent.release_tool_calls();

        // The cancel was already written to the wire the instant `Stop`
        // returned — cancellation is local and does not wait on the vendor.
        // Releasing the gate only lets the fake's own task get scheduled to
        // read it back off the socket and record it, which can lag this
        // task by a beat; poll rather than assert immediately.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !agent.cancelled_calls().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Stop must propagate a cancel to the runtime; the sandbox never heard \
                 about it (signals seen: {:?})",
                agent.signals()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        server.shutdown().await;
    })
    .await
    .expect("test timed out");
}

/// #61 item 3: answering an `ask_user` question leaves the session in
/// `AwaitingInput`, so a follow-up message starts a *second concurrent run on the
/// same agent and journal*.
///
/// `on_user_message`'s `AwaitingInput` branch injects the answer and returns
/// `CommandEffect::none()` — no `report(Running)`, no persisted event — so the
/// status stays `AwaitingInput` for the whole resumed turn. A second message
/// re-enters the same branch and issues a second `InjectToolResult`;
/// `AgentActor` has no concurrency guard, and `start_run` overwrites
/// `self.running` with a fresh token, orphaning the first run's cancel token.
/// Two background loops then persist interleaved events into one `agent/<id>`
/// journal, both injecting a `tool_result` for the *same* `tool_call_id` — the
/// duplicate-tool-result shape that makes the provider 400 on every later turn.
///
/// #62 added a client-side latch, so the browser cannot trigger this; the server
/// still accepts it, which any non-browser client can do.
#[tokio::test]
async fn a_session_runs_a_turn_against_a_connected_vendor_agent() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("hello from the agent");
    let tmp = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();

    let (server, _local_addr) = start_server_with_live_vendors(tmp.path(), &mock.url()).await;
    let agent = horsie_server::runtime_vendor::fake::FakeRuntimeVendor::builder("agent-1")
        .supports_provisioning(true)
        .bash_stdout("from-the-agent")
        .connect(&format!("ws://{}/api/vendor/connect", server.addr))
        .await
        .expect("agent connects");
    // Let the handshake land before the session resolves the vendor name.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let id = create_session_for_vendor(&client, &server.addr, "agent-1").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    assert_eq!(
        send_message(&client, &server.addr, &id, "hi")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server.addr, &id, "Idle").await;

    assert!(
        agent.signals().iter().any(|s| s.starts_with("create:")),
        "the agent must have been asked to create the runtime, saw {:?}",
        agent.signals()
    );
    assert_eq!(
        agent.live_runtimes().len(),
        1,
        "one runtime for one session"
    );
}

/// The agent is resident for the session's loaded lifetime, not spawned per
/// turn. So once a turn concludes, reading the session costs nothing: history
/// and usage are answered from the agent already in memory, and no read ever
/// asks the vendor for a runtime.
#[tokio::test]
async fn reads_after_a_concluded_turn_acquire_no_runtime() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first reply");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "hello").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    let after_turn = agent.signals();
    assert_eq!(
        after_turn,
        vec![
            format!("create:{id}"),
            format!("get:{id}"),
            format!("get:{id}")
        ],
        "one create at session creation, two gets for the first turn — the hook \
         seam resolves one before the snapshot, `provide` one for the run"
    );

    // Read it every way a client can, repeatedly.
    for _ in 0..3 {
        let page: serde_json::Value = client
            .get(format!(
                "http://{}/api/sessions/{id}/agents/main/history?limit=50",
                server.addr
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            !page_messages(&page).is_empty(),
            "the resident agent still holds the transcript: {page}"
        );
        let usage: serde_json::Value = client
            .get(format!(
                "http://{}/api/sessions/{id}/agents/main",
                server.addr
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            usage["agent"]["usage"]["inputTokens"].as_u64().unwrap() > 0,
            "the agent document reports its own usage: {usage}"
        );
        let _ = get_detail(&client, &server.addr, &id).await;
    }

    assert_eq!(
        agent.signals(),
        after_turn,
        "reading a session must cost no vendor call at all"
    );

    server.shutdown().await;
}

/// The promise a `202` makes: every accepted message is answered, and messages
/// accepted during one turn go in together as the next one.
#[tokio::test]
async fn messages_queued_during_a_turn_are_merged_into_the_next_one() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    let block = mock.blocking_response("first");
    send_message(&client, &server.addr, &id, "one").await;
    block.wait_until_received().await;

    for text in ["two", "three"] {
        assert_eq!(
            send_message(&client, &server.addr, &id, text)
                .await
                .as_u16(),
            202,
            "a message sent during a run is accepted, never refused"
        );
    }
    wait_inbox(&client, &server.addr, &id, &["two", "three"]).await;

    mock.queue_response("second");
    block.release();
    wait_inbox(&client, &server.addr, &id, &[]).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // One turn, one user message: consecutive user turns are not portable
    // across providers, so the queue is joined with a blank line instead.
    let page: serde_json::Value = client
        .get(format!(
            "http://{}/api/sessions/{id}/agents/main/history?limit=100",
            server.addr
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_texts: Vec<String> = page_messages(&page)
        .iter()
        .filter(|m| m["role"] == "User")
        .map(|m| {
            m["parts"][0]["value"]["text"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(user_texts, vec!["one", "two\n\nthree"], "{page}");

    server.shutdown().await;
}

/// A message is durable the moment it is accepted, so a crash mid-turn owes the
/// user an answer to it — and owes them no turn they did not start.
#[tokio::test]
async fn a_crash_keeps_the_inbox_and_starts_nothing_on_its_own() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    let block = mock.blocking_response("never delivered");
    send_message(&client, &server.addr, &id, "the turn that dies").await;
    block.wait_until_received().await;
    send_message(&client, &server.addr, &id, "still owed an answer").await;
    wait_inbox(&client, &server.addr, &id, &["still owed an answer"]).await;

    // Crash mid-turn, with the queue non-empty.
    server.shutdown().await;

    let signals_before = agent.signals();
    let server2 = start_server(tmp.path(), agent.link(), &mock.url()).await;
    wait_status(&client, &server2.addr, &id, "Idle").await;
    // The queue survived: the session actor recovers it from its journal, and
    // recovering acquires no runtime.
    wait_inbox(&client, &server2.addr, &id, &["still owed an answer"]).await;
    assert_eq!(
        agent.signals(),
        signals_before,
        "reading a session must acquire no runtime"
    );

    // And nothing ran on its own: the queued message is still queued, waiting
    // for the user to come back rather than being answered behind their back.
    tokio::time::sleep(Duration::from_millis(200)).await;
    wait_inbox(&client, &server2.addr, &id, &["still owed an answer"]).await;

    server2.shutdown().await;
}

/// The two runtime failures the design draws a hard line between: a vendor that
/// says the runtime is gone ends the session, while a vendor that is merely
/// unreachable fails one turn.
#[tokio::test]
async fn a_gone_runtime_is_terminal_while_an_unreachable_vendor_is_not() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    // The agent creates happily and then reports the runtime gone on every get.
    let agent = FakeRuntimeVendor::builder("mock")
        .gone_on_get(true)
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    mock.queue_response("never reached");
    send_message(&client, &server.addr, &id, "hello").await;
    wait_status(&client, &server.addr, &id, "Unrecoverable").await;

    // Terminal means terminal: further messages are refused rather than
    // silently rebuilding a workspace the user believes they still have.
    let res = client
        .post(format!("http://{}/api/sessions/{id}/messages", server.addr))
        .json(&serde_json::json!({ "text": "try again" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status().as_u16(),
        409,
        "a dead session refuses messages"
    );
    assert_eq!(
        agent
            .signals()
            .iter()
            .filter(|s| s.starts_with("create:"))
            .count(),
        1,
        "never a second create: {:?}",
        agent.signals()
    );

    server.shutdown().await;
}

#[tokio::test]
async fn an_unreachable_vendor_fails_one_turn_and_recovers_on_the_next() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();

    let (server, _local) = start_server_with_live_vendors(tmp.path(), &mock.url()).await;
    let url = format!("ws://{}/api/vendor/connect", server.addr);
    let agent = FakeRuntimeVendor::builder("agent-3")
        .connect(&url)
        .await
        .expect("agent connects");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let id = create_session_for_vendor(&client, &server.addr, "agent-3").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // The vendor goes away between turns. Its runtime is not gone — nobody can
    // say either way — so the turn fails and the session stays usable.
    agent.disconnect();
    tokio::time::sleep(Duration::from_millis(150)).await;
    mock.queue_response("never reached");
    send_message(&client, &server.addr, &id, "while the vendor is down").await;
    wait_status(&client, &server.addr, &id, "Failed").await;

    // The vendor comes back, still owning the runtimes it created — a real
    // vendor's sandboxes outlive its agent process.
    let agent2 = FakeRuntimeVendor::builder("agent-3")
        .resuming(&agent)
        .connect(&url)
        .await
        .expect("agent reconnects");
    tokio::time::sleep(Duration::from_millis(150)).await;
    mock.queue_response("back in business");
    assert_eq!(
        send_message(&client, &server.addr, &id, "and now?")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server.addr, &id, "Idle").await;
    let signals = agent2.signals();
    assert!(
        signals.iter().any(|s| s == &format!("get:{id}")),
        "the recovered turn resumes the same runtime: {signals:?}"
    );
    assert_eq!(
        signals.iter().filter(|s| s.starts_with("create:")).count(),
        1,
        "resuming must never provision a second runtime: {signals:?}"
    );
}

/// The idle clock, through the full HTTP stack: a session left alone is
/// unloaded and its runtime hibernated, and the next message resumes that same
/// runtime rather than building another.
#[tokio::test]
async fn an_idle_session_hibernates_and_the_next_message_resumes_it() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first");
    mock.queue_response("second");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let clock = Arc::new(TestClock::new());
    let server =
        start_server_with(tmp.path(), agent.link(), &mock.url(), Some(clock.clone())).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr).await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Nothing happens on its own — the clock has not moved.
    let _ = server.supervisor.tell(SessionSupervisorCommand::Tick).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !agent.signals().contains(&format!("hibernate:{id}")),
        "a session inside its idle window must stay loaded: {:?}",
        agent.signals()
    );

    clock.advance(Duration::from_secs(600));
    let _ = server.supervisor.tell(SessionSupervisorCommand::Tick).await;
    for _ in 0..100 {
        if agent.signals().contains(&format!("hibernate:{id}")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        agent.signals().contains(&format!("hibernate:{id}")),
        "an idle session is unloaded and its runtime hibernated: {:?}",
        agent.signals()
    );
    // Unloading loses nothing a reader can see: opening the session again
    // reports the status its journal recorded.
    wait_status(&client, &server.addr, &id, "Idle").await;

    assert_eq!(
        send_message(&client, &server.addr, &id, "two")
            .await
            .as_u16(),
        202
    );
    wait_status(&client, &server.addr, &id, "Idle").await;
    assert_eq!(
        agent
            .signals()
            .iter()
            .filter(|s| s.starts_with("create:"))
            .count(),
        1,
        "a resumed session reuses its runtime; it is created exactly once: {:?}",
        agent.signals()
    );
    assert!(
        agent
            .signals()
            .iter()
            .filter(|s| *s == &format!("get:{id}"))
            .count()
            >= 2,
        "the resumed turn asked for the same runtime: {:?}",
        agent.signals()
    );

    server.shutdown().await;
}

/// Stopping one session must not disturb another on the same agent — the
/// inverse of the shared-local daemon, where `stop` had to be a no-op precisely
/// because it would have hit every session at once.
#[tokio::test]
async fn stopping_one_session_leaves_another_on_the_same_agent_alive() {
    let mock = MockLlmServer::builder().build().await;
    for _ in 0..4 {
        mock.queue_response("ok");
    }
    let tmp = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();

    let (server, _local_addr) = start_server_with_live_vendors(tmp.path(), &mock.url()).await;
    let agent = horsie_server::runtime_vendor::fake::FakeRuntimeVendor::builder("agent-2")
        .bash_stdout("ok")
        .connect(&format!("ws://{}/api/vendor/connect", server.addr))
        .await
        .expect("agent connects");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let a = create_session_for_vendor(&client, &server.addr, "agent-2").await;
    let b = create_session_for_vendor(&client, &server.addr, "agent-2").await;
    wait_status(&client, &server.addr, &a, "Idle").await;
    wait_status(&client, &server.addr, &b, "Idle").await;

    send_message(&client, &server.addr, &a, "hi").await;
    wait_status(&client, &server.addr, &a, "Idle").await;
    send_message(&client, &server.addr, &b, "hi").await;
    wait_status(&client, &server.addr, &b, "Idle").await;
    assert_eq!(agent.live_runtimes().len(), 2, "one runtime per session");

    client
        .post(format!("http://{}/api/sessions/{a}/stop", server.addr))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    wait_status(&client, &server.addr, &a, "Idle").await;

    // Hibernate is advisory and this agent declines it, so both runtimes are
    // still there. What matters is that stopping one session did not disturb
    // the other's runtime — which the message below proves.
    assert!(
        agent.live_runtimes().contains(&b),
        "the untouched session must keep its runtime"
    );
    assert_eq!(
        send_message(&client, &server.addr, &b, "again")
            .await
            .as_u16(),
        202,
        "session b must be unaffected by a's stop"
    );
    wait_status(&client, &server.addr, &b, "Idle").await;
}
