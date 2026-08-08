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

use horsie_actor::ActorRef;
use horsie_agentcore::LlmProvider;
use horsie_anthropic::AnthropicProvider;
use horsie_mock_llm::MockLlmServer;
use horsie_server::db::Db;
use horsie_server::http::{AppState, app};
use horsie_server::runtime_vendor::RuntimeVendorLink;
use horsie_server::runtime_vendor::fake::FakeRuntimeVendor;
use horsie_server::sessions::clock::TestClock;
use horsie_server::sessions::supervisor::{SessionSupervisorCommand, SupervisorConfig};
use horsie_server::users::{Shared, UserRegistry};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The LLM messages on a `/messages` page.
///
/// A page is a list of log *entries*, each carrying a tagged body — a hook
/// record and a session lifecycle event are entries too, and neither is a
/// message. Tests that reason about the conversation go through here so a new
/// body kind cannot silently change what they count.
fn page_messages(page: &serde_json::Value) -> Vec<serde_json::Value> {
    page["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("a messages page must carry entries: {page}"))
        .iter()
        .filter(|e| e["body"]["type"] == serde_json::json!("Llm"))
        .map(|e| e["body"]["value"].clone())
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
    start_server_with(journal_dir, Some(vendor), mock_url, None).await
}

/// As [`start_server`], but with the supervisor's idle policy under the test's
/// control: a clock that only moves when told, and no background ticker, so
/// offload happens exactly when the test sends `Tick` and never by surprise.
async fn start_server_with(
    journal_dir: &Path,
    vendor: Option<Arc<RuntimeVendorLink>>,
    mock_url: &str,
    clock: Option<Arc<TestClock>>,
) -> Server {
    start_server_on(journal_dir, vendor, provider_at(mock_url), clock).await
}

/// As [`start_server_with`], but with the LLM provider chosen by the caller —
/// the seam a test needs to drive a wire other than Anthropic's.
async fn start_server_on(
    journal_dir: &Path,
    vendor: Option<Arc<RuntimeVendorLink>>,
    provider: Arc<dyn LlmProvider>,
    clock: Option<Arc<TestClock>>,
) -> Server {
    // The e2e suite runs on the production default backend, so every test here
    // — including the restart ones — exercises real snapshots and compaction.
    let db = Db::open(&format!("sqlite://{}/config.db", journal_dir.display()), 5)
        .await
        .unwrap();
    // Auth off: this suite drives the HTTP API without a credential, and a
    // disabled deployment is a supported configuration. Authenticated coverage
    // lives in the server crate's own HTTP tests.
    let auth = Arc::new(horsie_server::auth::AuthService::new(
        horsie_server::auth::AuthStore::new(db.clone()),
        horsie_server::auth::AuthDeps {
            mode: horsie_server::auth::AuthMode::Off,
            state_dir: journal_dir.to_path_buf(),
        },
    ));
    auth.bootstrap().await.unwrap();
    let account = auth.sole_user().await.unwrap().expect("bootstrapped");

    let shared = Arc::new(Shared {
        db,
        artifacts: Arc::new(horsie_server::plugins::ArtifactStore::new(
            journal_dir.join("plugin-artifacts"),
        )),
        artifact_secret: Arc::new(b"e2e-secret".to_vec()),
        info: horsie_models::settings::ServerInfo {
            config_path: String::new(),
            database: String::new(),
            state_dir: String::new(),
            data_dir: String::new(),
            plugins_dir: String::new(),
            version: "test".into(),
        },
        model_card_seed: Arc::new(Vec::new()),
        model_card_seed_marker: horsie_server::config::model_cards::seed_marker(&[]),
        anonymous: account.clone(),
        // With a clock the test drives the idle policy itself: no background
        // ticker, so an offload happens exactly when it sends `Tick`.
        supervisor: match clock {
            Some(clock) => SupervisorConfig {
                clock,
                idle_timeout: Duration::from_secs(180),
                tick_interval: None,
            },
            None => SupervisorConfig::default(),
        },
    });
    let users = Arc::new(UserRegistry::new(shared.clone()));
    let services = users.get(&account).await.unwrap();

    // The doubles go into the account's own registry and vendor map — the same
    // handles its supervisor and runtime manager read.
    services
        .provider_registry
        .write()
        .unwrap()
        .insert("mock".into(), provider);
    if let Some(vendor) = vendor {
        services
            .vendors
            .write()
            .unwrap()
            .insert("mock".into(), vendor);
    }

    let supervisor = services.supervisor.clone();
    let state = AppState {
        auth,
        shared,
        users,
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

/// Create a session and wait until its runtime actually exists.
///
/// A session now says `Provisioning` until its vendor confirms the runtime, so
/// leaving that status is itself the answer — but these tests take vendors away
/// and restart servers, and the vendor's own signal is the shortest statement of
/// "the create happened". Every test here means "a session that is up", so this
/// is where that is established, once.
async fn create_session(
    client: &reqwest::Client,
    addr: &SocketAddr,
    agent: &FakeRuntimeVendor,
    message: &str,
) -> String {
    let body = serde_json::json!({
        "agent": { "model": "mock", "use_plugins": false },
        "vendor": "mock",
        "message": message
    });
    let res = client
        .post(format!("http://{addr}/api/sessions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let v: serde_json::Value = res.json().await.unwrap();
    let id = v["session"]["id"].as_str().unwrap().to_string();
    wait_for_signal(agent, &format!("create:{id}")).await;
    id
}

/// Like `create_session`, but selects a named vendor with no `repos` — the
/// shape a shared-local-vendor session must use (it provisions nothing).
async fn create_session_for_vendor(
    client: &reqwest::Client,
    addr: &SocketAddr,
    vendor: &str,
    agent: &FakeRuntimeVendor,
    message: &str,
) -> String {
    let body = serde_json::json!({
        "agent": { "model": "mock", "use_plugins": false },
        "vendor": vendor,
        "message": message
    });
    let res = client
        .post(format!("http://{addr}/api/sessions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let v: serde_json::Value = res.json().await.unwrap();
    let id = v["session"]["id"].as_str().unwrap().to_string();
    // As `create_session`: a session is not up until its runtime exists.
    wait_for_signal(agent, &format!("create:{id}")).await;
    id
}

/// Start a server with no vendor pre-published, for the tests where an agent
/// dials `/api/vendor/connect` and has to become resolvable by session creation
/// exactly as in production.
///
/// That used to need its own harness, because the old one built a vendor map
/// separate from the one the config store handed out. An account's map is now
/// the only one there is, so this is `start_server_with` with nothing in it.
async fn start_server_with_live_vendors(
    journal_dir: &Path,
    mock_url: &str,
) -> (Server, SocketAddr) {
    let server = start_server_with(journal_dir, None, mock_url, None).await;
    let addr = server.addr;
    (server, addr)
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

/// An agent's queue, folded from its log exactly as a client folds it:
/// every `MessageQueued` entry, minus the ones a later `TurnBegan` consumed.
///
/// There is no second source to read it from. The queue belongs to the agent
/// the message is addressed to, and its log is where that agent says so —
/// which is what makes the queue and the transcript around it one ordered
/// thing rather than two that have to be reconciled.
fn queued_texts(page: &serde_json::Value) -> Vec<String> {
    let entries = page["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("a messages page must carry entries: {page}"));
    let mut queue: Vec<(String, String)> = Vec::new();
    for entry in entries {
        let body = &entry["body"];
        if body["type"] != serde_json::json!("Lifecycle") {
            continue;
        }
        let value = &body["value"];
        match value["kind"].as_str() {
            Some("MessageQueued") => {
                let m = &value["value"];
                queue.push((
                    m["id"].as_str().unwrap_or_default().to_string(),
                    m["text"].as_str().unwrap_or_default().to_string(),
                ));
            }
            Some("TurnBegan") => {
                let consumed: Vec<&str> = value["value"]["consumed"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                queue.retain(|(id, _)| !consumed.contains(&id.as_str()));
            }
            _ => {}
        }
    }
    queue.into_iter().map(|(_, text)| text).collect()
}

/// One agent's whole log, newest window, as a page.
async fn messages_page(
    client: &reqwest::Client,
    addr: &SocketAddr,
    id: &str,
    aid: &str,
) -> serde_json::Value {
    client
        .get(format!(
            "http://{addr}/api/sessions/{id}/messages?aid={aid}&max=200"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Poll the main agent's log until its queue holds exactly `want` texts.
async fn wait_inbox(client: &reqwest::Client, addr: &SocketAddr, id: &str, want: &[&str]) {
    let deadline = Duration::from_secs(10);
    let start = std::time::Instant::now();
    loop {
        let got = queued_texts(&messages_page(client, addr, id, "main").await);
        if got == want {
            return;
        }
        if start.elapsed() > deadline {
            panic!("timed out waiting for queue {want:?}; last = {got:?}");
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

/// Poll a fake agent's signals until `signal` appears, or the deadline passes.
///
/// A create legitimately runs for minutes, so the session runs it off its own
/// mailbox and stays `Provisioning` until it has an answer. A test that takes
/// the vendor away, or restarts the server, has to wait for `create:<id>` first
/// or it is racing the create it meant to happen before.
async fn wait_for_signal(agent: &FakeRuntimeVendor, signal: &str) {
    let deadline = Duration::from_secs(10);
    let start = std::time::Instant::now();
    loop {
        if agent.signals().iter().any(|s| s == signal) {
            return;
        }
        assert!(
            start.elapsed() <= deadline,
            "timed out waiting for {signal}; saw {:?}",
            agent.signals()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
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

/// The `kind` of every lifecycle entry on a stream, in order.
///
/// Session-scoped facts are entries in the agent's log now, not frames of their
/// own — so a test that used to look for an `InboxChanged` frame looks for a
/// `MessageQueued` entry instead, at a known position relative to everything
/// else rather than on a separate stream with no ordering against it.
fn stream_lifecycle(events: &[Ev]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind == "Entry")
        .filter(|e| e.data["value"]["body"]["type"] == serde_json::json!("Lifecycle"))
        .filter_map(|e| {
            e.data["value"]["body"]["value"]["kind"]
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// The lifecycle entries of one kind, with their payloads.
fn stream_lifecycle_values(events: &[Ev], kind: &str) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter(|e| e.kind == "Entry")
        .filter(|e| e.data["value"]["body"]["type"] == serde_json::json!("Lifecycle"))
        .filter(|e| e.data["value"]["body"]["value"]["kind"] == serde_json::json!(kind))
        .map(|e| e.data["value"]["body"]["value"]["value"].clone())
        .collect()
}

// ── tests ────────────────────────────────────────────────────────────────────

/// horsie#232, end to end: the message a session is created with must not
/// outrun the create it rides on.
///
/// The vendor holds every create open, so the whole window is under the test's
/// control rather than a scheduling accident. Inside it the session has to
/// report `Provisioning` and ask the vendor for nothing; released, it has to
/// run the message it was carrying all along.
#[tokio::test]
async fn a_first_turn_waits_for_the_create_it_rides_on() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("answered once the runtime was up");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .block_creates()
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "agent": { "model": "mock", "use_plugins": false },
        "vendor": "mock",
        "message": "hello"
    });
    let res = client
        .post(format!("http://{}/api/sessions", server.addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let v: serde_json::Value = res.json().await.unwrap();
    let id = v["session"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        v["session"]["status"], "Provisioning",
        "a session is not idle before it has a runtime"
    );

    wait_status(&client, &server.addr, &id, "Provisioning").await;
    assert!(
        !agent.signals().iter().any(|s| s.starts_with("get:")),
        "nothing may ask the vendor for a runtime it has not been told to build; saw {:?}",
        agent.signals()
    );

    agent.release_creates();
    wait_status(&client, &server.addr, &id, "Idle").await;
    let page: serde_json::Value = client
        .get(format!(
            "http://{}/api/sessions/{id}/messages?aid=main&max=50",
            server.addr
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = serde_json::to_string(&page_messages(&page)).unwrap();
    assert!(
        text.contains("answered once the runtime was up"),
        "the message the session was created with is still owed an answer: {text}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn create_message_sse_roundtrip() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("the turn that made the session");
    mock.queue_response("hello from the agent");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    // A cursorless connect replays the whole log and then goes live, so this
    // sees the create's own turn as well as the one it sends. That is the
    // change: there is no longer a "backfill, then subscribe" seam for a turn
    // to fall through, because the read and the stream are the same request.
    let id = create_session(&client, &server.addr, &agent, "first").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    let url = format!("http://{}/api/sessions/{id}/messages?aid=main", server.addr);
    let client2 = client.clone();
    let sse = tokio::spawn(async move {
        collect_sse(&client2, &url, None, |evs| {
            // Two: the create's turn, replayed, and the one sent below.
            stream_lifecycle(evs)
                .iter()
                .filter(|k| *k == "TurnEnded")
                .count()
                >= 2
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
    assert!(ks.contains(&"Entry".to_string()), "kinds: {ks:?}");
    let lifecycle = stream_lifecycle(&events);
    assert!(
        lifecycle.iter().any(|k| k == "TurnEnded"),
        "the turn's end is an entry in the log, in order with the messages it \
         followed rather than on a stream of its own: {lifecycle:?}"
    );
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
    // Every frame that holds a *position* carries an SSE id — entries and
    // deltas alike, so a reconnect can resume from any of them rather than only
    // from the last durable append. The window frame is the one exception: it
    // describes the window that follows rather than sitting in it, so giving it
    // an id would let a reconnect resume from somewhere nothing was received.
    let positioned: Vec<&Ev> = events.iter().filter(|e| e.kind != "Window").collect();
    let ids: Vec<String> = positioned.iter().filter_map(|e| e.id.clone()).collect();
    assert_eq!(
        ids.len(),
        positioned.len(),
        "every positioned frame is resumable"
    );
    assert!(
        events
            .iter()
            .filter(|e| e.kind == "Window")
            .all(|e| e.id.is_none()),
        "the window frame holds no position"
    );
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "ids must be unique: {ids:?}");
    // Entry ids are plain integers and strictly increase; a delta's is
    // `<entry>.<n>`, which is what keeps it ordered against the entry it
    // follows without competing for the same number.
    let entry_seqs: Vec<u64> = events
        .iter()
        .filter(|e| e.kind == "Entry")
        .filter_map(|e| e.id.as_ref()?.parse().ok())
        .collect();
    assert!(
        entry_seqs.windows(2).all(|w| w[0] < w[1]),
        "entry ids must strictly increase: {entry_seqs:?}"
    );

    wait_status(&client, &server.addr, &id, "Idle").await;
    assert_eq!(
        agent.signals(),
        vec![
            format!("create:{id}"),
            format!("get:{id}"),
            format!("get:{id}"),
            format!("get:{id}")
        ],
        "one create at session creation, then two gets for the first turn: the \
         pre-run hook seam needs a runtime before the turn snapshots its \
         history, and `provide` still resolves one of its own so a hibernated \
         runtime is resumed on every run. The second turn reuses the cached \
         handle and costs one. `get` never provisions."
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_queued_message_is_visible_on_the_agents_log_and_its_stream() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    // A turn that hangs inside the provider call, so the session is genuinely
    // Running when the second message arrives. Armed before the create, since
    // the create is what starts that turn.
    let block = mock.blocking_response("first");
    let id = create_session(&client, &server.addr, &agent, "one").await;
    block.wait_until_received().await;

    // Subscribe while the turn is in flight — this stands in for a second tab,
    // which must learn about the queue without reloading the page. The queue is
    // the agent's, so it rides that agent's log like everything else.
    let url = format!("http://{}/api/sessions/{id}/messages?aid=main", server.addr);
    let client2 = client.clone();
    let sse = tokio::spawn(async move {
        collect_sse(&client2, &url, None, |evs| {
            // Two: the create's own message, replayed, and the one queued
            // mid-turn below.
            stream_lifecycle(evs)
                .iter()
                .filter(|k| *k == "MessageQueued")
                .count()
                >= 2
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

    // The agent's own log is the durable source of the queue. The message that
    // created the session has already been taken into the turn in flight, so
    // only the one just sent is still owed.
    wait_inbox(&client, &server.addr, &id, &["two"]).await;
    let page = messages_page(&client, &server.addr, &id, "main").await;
    let ids: Vec<&str> = page["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|e| e["body"]["value"]["kind"] == serde_json::json!("MessageQueued"))
        .filter_map(|e| e["body"]["value"]["value"]["id"].as_str())
        .collect();
    assert!(
        ids.iter().all(|id| !id.is_empty()),
        "a queued entry carries the id the send acknowledged: {ids:?}"
    );

    // ... and the live stream says the same thing, on the same connection as
    // the transcript rather than a second one with no order against it.
    let events = sse.await.unwrap();
    // The replay carries the create's own message too, so the one under test is
    // the last — and the log is what makes "last" meaningful without a second
    // source to compare against.
    let queued = stream_lifecycle_values(&events, "MessageQueued");
    assert_eq!(
        queued.last().expect("a queued entry")["text"],
        serde_json::json!("two"),
        "queued entries: {queued:?}"
    );

    // Letting the turn finish carries the message out of the queue.
    mock.queue_response("second");
    block.release();
    wait_inbox(&client, &server.addr, &id, &[]).await;

    server.shutdown().await;
}

#[tokio::test]
async fn prep_progressions_stream_during_a_turn() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first");
    mock.queue_response("done");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    // The session's own first turn is not the one under test: progression
    // frames are live-only, so this test needs a turn it can subscribe *before*,
    // and only a second message gives it one.
    let id = create_session(&client, &server.addr, &agent, "warm up").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    // Subscribe before sending so the live progression frames are seen. Prep
    // is session-scoped, so it streams on the session — and the turn's end is
    // observed there as the status returning to Idle.
    let url = format!("http://{}/api/sessions/{id}/messages?aid=main", server.addr);
    let client2 = client.clone();
    let sse = tokio::spawn(async move {
        collect_sse(&client2, &url, None, |evs| {
            stream_lifecycle(evs).iter().any(|k| k == "TurnEnded")
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    send_message(&client, &server.addr, &id, "hi").await;

    let events = sse.await.unwrap();
    // Preparation stages are `Preparing` entries in the log, before the reply.
    // Journaled rather than live-only, so a client that connects
    // mid-preparation still learns what happened. Distinct from `Runtime`,
    // which is the session's sandbox rather than this turn's setup — the two
    // used to share one variant and both used the stage "ready".
    let stages: Vec<String> = stream_lifecycle_values(&events, "Preparing")
        .iter()
        .filter_map(|v| v["stage"].as_str().map(str::to_string))
        .collect();
    assert!(
        stages.iter().any(|s| s == "scanning_workspace"),
        "missing scanning_workspace progression: {stages:?}"
    );
    assert!(
        stages.iter().any(|s| s == "ready"),
        "missing ready progression: {stages:?}"
    );
    // The session's own runtime is its own variant, so a consumer acting on
    // "the sandbox is up" cannot be fooled by a turn finishing its setup.
    let runtime: Vec<String> = stream_lifecycle_values(&events, "Runtime")
        .iter()
        .filter_map(|v| v["status"]["kind"].as_str().map(str::to_string))
        .collect();
    assert!(
        runtime.iter().any(|s| s == "Ready"),
        "the sandbox landing is its own fact: {runtime:?}"
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

    let id = create_session(&client, &server.addr, &agent, "one").await;

    // Two completed turns → user + assistant per turn = 4 messages. Poll the
    // history until both turns have landed (status can read `Idle` between the
    // 202 and the turn flipping to `Running`, so a bare `wait_status` races).
    let history = |limit: u32, before: Option<String>| {
        let client = client.clone();
        let addr = server.addr;
        let id = id.clone();
        async move {
            let mut url = format!("http://{addr}/api/sessions/{id}/messages?aid=main&max={limit}");
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
    let history_before = |limit: usize, before: u64| {
        let client = client.clone();
        let addr = server.addr;
        let id = id.clone();
        async move {
            client
                .get(format!(
                    "http://{addr}/api/sessions/{id}/messages?aid=main&max={limit}&before={before}"
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
    // queued and merged into the next turn, so wait for turn one — the one the
    // create started — to land its reply before sending turn two.
    wait_for(2).await;
    send_message(&client, &server.addr, &id, "two").await;
    wait_for(4).await;

    // Tail page with a small limit. A page is a window of *entries* now, not of
    // messages: lifecycle entries share the log and share the numbering, which
    // is the whole point of one ordered thing. So the limit is asserted on
    // entries, and the reply is found by widening the window.
    let page = history(2, None).await;
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "tail limit not honored: {page}");
    assert!(
        page.get("hasMoreBefore").is_none() && page.get("hasMoreAfter").is_none(),
        "fewer entries than asked for is how a client learns there are no more: {page}"
    );
    // The newest assistant reply is in a wide enough tail window.
    let wide = history(100, None).await;
    assert!(
        wide.to_string().contains("second reply"),
        "tail missing latest: {wide}"
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

    // Scroll back from a known seq. The cursor is the entry's own number now,
    // not its id — which is what turns a lookup that used to scan the whole log
    // into a binary search, and lets two entries be compared without it.
    let all_entries = all["entries"].as_array().unwrap();
    let anchor = all_entries[4]["seq"].as_u64().unwrap();
    let older = history_before(3, anchor).await;
    let older_seqs: Vec<u64> = older["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        older_seqs,
        vec![anchor - 3, anchor - 2, anchor - 1],
        "a scroll-back window ends just before its cursor and excludes it: {older}"
    );

    // A cursor the log does not hold is answered with nothing rather than a
    // silently-wrong window — the caller must re-seed.
    let missing = history_before(3, 999_999).await;
    assert!(
        missing["entries"].as_array().unwrap().is_empty(),
        "an unresolvable cursor owes nothing: {missing}"
    );

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
    let id = create_session(&client, &server.addr, &agent, "one").await;

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

    // Turn one is the session's own creation; poll until its usage has landed
    // (the create returns before the turn completes). There is no zeroed
    // reading to take first — a session is created with a message, so it has
    // always spent something by the time anyone can read it.
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

    let id = create_session(&client, &server.addr, &agent, "one").await;
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
                    "http://{addr}/api/sessions/{id}/messages?aid=main&max=100"
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

    let id = create_session(&client, &server.addr, &agent, "one").await;
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
    // A blocking turn: the LLM request arrives, then hangs — the session is
    // Running when we simulate a crash. The session's first turn is that turn.
    let block = mock.blocking_response("never delivered");
    let id = create_session(&client, &server.addr, &agent, "hang").await;
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

    let id = create_session(&client, &server.addr, &agent, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "two").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    let url = format!("http://{}/api/sessions/{id}/messages?aid=main", server.addr);

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
        "message": "hi",
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
        "vendor": "mock",
        "message": "hi"
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
        "vendor": "mock",
        "message": "hi"
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
        "vendor": "mock",
        "message": "hi"
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
        mock.queue_tool_call("bash", serde_json::json!({ "command": "echo hi" }));
        mock.queue_response("done anyway");
        let id = create_session(&client, &server.addr, &agent, "first").await;

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
        mock.queue_tool_call("bash", serde_json::json!({ "command": "sleep 999" }));
        let id = create_session(&client, &server.addr, &agent, "run something slow").await;
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

    let id = create_session_for_vendor(&client, &server.addr, "agent-1", &agent, "hi").await;
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

    let id = create_session(&client, &server.addr, &agent, "hello").await;
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
                "http://{}/api/sessions/{id}/messages?aid=main&max=50",
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

    let block = mock.blocking_response("first");
    let id = create_session(&client, &server.addr, &agent, "one").await;
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
            "http://{}/api/sessions/{id}/messages?aid=main&max=100",
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

    let block = mock.blocking_response("never delivered");
    let id = create_session(&client, &server.addr, &agent, "the turn that dies").await;
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

    mock.queue_response("never reached");
    let id = create_session(&client, &server.addr, &agent, "hello").await;
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

    let id = create_session_for_vendor(&client, &server.addr, "agent-3", &agent, "first").await;
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
    let server = start_server_with(
        tmp.path(),
        Some(agent.link()),
        &mock.url(),
        Some(clock.clone()),
    )
    .await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr, &agent, "one").await;
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

    let a = create_session_for_vendor(&client, &server.addr, "agent-2", &agent, "hi").await;
    let b = create_session_for_vendor(&client, &server.addr, "agent-2", &agent, "hi").await;
    wait_status(&client, &server.addr, &a, "Idle").await;
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

// ── prompt-cache prefix stability ────────────────────────────────────────────

/// Every request the mock saw, oldest first.
async fn received(client: &reqwest::Client, mock: &MockLlmServer) -> Vec<serde_json::Value> {
    let mut v: Vec<serde_json::Value> = client
        .get(format!("{}/received", mock.url()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v.reverse(); // the endpoint hands them back most-recent-first
    v
}

/// Strip the moving cache breakpoint, which is *supposed* to move.
fn without_cache_control(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => serde_json::Value::Object(
            m.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, v)| (k.clone(), without_cache_control(v)))
                .collect(),
        ),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(without_cache_control).collect())
        }
        other => other.clone(),
    }
}

/// A backend serves a prompt from cache only when the new request repeats the
/// previous one exactly and appends to it. Measured against the live ChatGPT
/// backend, a 114k-token prefix in this shape is served at 99% on every repeat
/// call — so a production cache miss is horsie's own prefix moving.
#[tokio::test]
async fn the_request_prefix_only_ever_grows() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first");
    mock.queue_response("second");
    mock.queue_response("third");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr, &agent, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    for text in ["two", "three"] {
        assert_eq!(
            send_message(&client, &server.addr, &id, text)
                .await
                .as_u16(),
            202
        );
        wait_status(&client, &server.addr, &id, "Idle").await;
    }

    let bodies = received(&client, &mock).await;
    assert!(bodies.len() >= 3, "expected 3 calls, got {}", bodies.len());

    for (n, pair) in bodies.windows(2).enumerate() {
        let (prev, next) = (&pair[0], &pair[1]);
        assert_eq!(
            prev["system"],
            next["system"],
            "call {n} -> {}: the system prompt changed, invalidating the whole prefix",
            n + 1
        );
        assert_eq!(
            without_cache_control(&prev["tools"]),
            without_cache_control(&next["tools"]),
            "call {n} -> {}: the tool list changed, invalidating the whole prefix",
            n + 1
        );
        let a = without_cache_control(&prev["messages"]);
        let b = without_cache_control(&next["messages"]);
        let (a, b) = (a.as_array().unwrap(), b.as_array().unwrap());
        for (i, old) in a.iter().enumerate() {
            assert_eq!(
                b.get(i),
                Some(old),
                "call {n} -> {}: message {i} of {} was rewritten instead of appended to",
                n + 1,
                a.len()
            );
        }
    }
}

/// The same, across a restart. A production session is offloaded and rehydrated
/// between turns, so the history the next request replays is one that came back
/// out of the journal — not the one still in memory. Anything the round trip
/// changes moves the prefix and costs the whole cached window.
#[tokio::test]
async fn the_request_prefix_survives_a_restart() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("first");
    mock.queue_response("second");
    mock.queue_response("third");
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();

    let id = create_session(&client, &server.addr, &agent, "one").await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    send_message(&client, &server.addr, &id, "two").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    server.shutdown().await;
    let server2 = start_server(tmp.path(), agent.link(), &mock.url()).await;
    send_message(&client, &server2.addr, &id, "three").await;
    wait_status(&client, &server2.addr, &id, "Idle").await;

    let bodies = received(&client, &mock).await;
    let (before, after) = (&bodies[bodies.len() - 2], &bodies[bodies.len() - 1]);
    assert_eq!(
        before["system"], after["system"],
        "the system prompt changed across a restart, invalidating the whole prefix"
    );
    let a = without_cache_control(&before["messages"]);
    let b = without_cache_control(&after["messages"]);
    let (a, b) = (a.as_array().unwrap(), b.as_array().unwrap());
    for (i, old) in a.iter().enumerate() {
        assert_eq!(
            b.get(i),
            Some(old),
            "message {i} of {} was rewritten across the restart",
            a.len()
        );
    }

    server2.shutdown().await;
}

/// The shape production actually runs: turns with tool calls, so one turn makes
/// several provider calls, each re-sending everything before it.
#[tokio::test]
async fn the_request_prefix_only_grows_across_tool_calls() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();
    // Two turns, each of them three provider calls: tool, tool, answer. The
    // first turn is the one the create starts, so its script is queued first.
    let script = || {
        mock.queue_tool_call("bash", serde_json::json!({ "command": "echo one" }));
        mock.queue_tool_call("bash", serde_json::json!({ "command": "echo two" }));
        mock.queue_response("done");
    };
    script();
    let id = create_session(&client, &server.addr, &agent, "first").await;
    wait_status(&client, &server.addr, &id, "Idle").await;
    script();
    send_message(&client, &server.addr, &id, "second").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    let bodies = received(&client, &mock).await;
    assert!(bodies.len() >= 6, "expected 6 calls, got {}", bodies.len());

    for (n, pair) in bodies.windows(2).enumerate() {
        let (prev, next) = (&pair[0], &pair[1]);
        assert_eq!(
            prev["system"],
            next["system"],
            "call {n} -> {}: the system prompt changed, invalidating the whole prefix",
            n + 1
        );
        assert_eq!(
            without_cache_control(&prev["tools"]),
            without_cache_control(&next["tools"]),
            "call {n} -> {}: the tool list changed, invalidating the whole prefix",
            n + 1
        );
        let a = without_cache_control(&prev["messages"]);
        let b = without_cache_control(&next["messages"]);
        let (a, b) = (a.as_array().unwrap(), b.as_array().unwrap());
        for (i, old) in a.iter().enumerate() {
            assert_eq!(
                b.get(i),
                Some(old),
                "call {n} -> {}: message {i} of {} was rewritten instead of appended to",
                n + 1,
                a.len()
            );
        }
    }

    server.shutdown().await;
}

/// The Responses wire, which is where the ChatGPT plan runs. Reasoning replay is
/// the part unique to it: the model only sees its own prior chain of thought if
/// horsie hands the encrypted item back, and an item that fails to come back
/// identical moves the prefix for every turn after it.
#[tokio::test]
async fn the_responses_prefix_only_grows_with_reasoning_replayed() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let provider: Arc<dyn LlmProvider> = Arc::new(
        horsie_openai_responses::ResponsesProvider::with_api_key("test-key")
            .unwrap()
            .with_model("mock")
            .with_base_url(mock.url()),
    );
    let server = start_server_on(tmp.path(), Some(agent.link()), provider, None).await;
    let client = reqwest::Client::new();
    mock.queue_reasoning("weighing it up", "done");
    let id = create_session(&client, &server.addr, &agent, "first").await;
    wait_status(&client, &server.addr, &id, "Idle").await;

    for text in ["second", "third"] {
        mock.queue_reasoning("weighing it up", "done");
        send_message(&client, &server.addr, &id, text).await;
        wait_status(&client, &server.addr, &id, "Idle").await;
    }

    let bodies = received(&client, &mock).await;
    assert!(bodies.len() >= 3, "expected 3 calls, got {}", bodies.len());
    // The point of the test: reasoning came back, so it can move the prefix.
    let last = bodies.last().unwrap();
    let kinds: Vec<&str> = last["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["type"].as_str())
        .collect();
    assert!(
        kinds.contains(&"reasoning"),
        "no reasoning item was replayed, so this proves nothing: {kinds:?}"
    );

    for (n, pair) in bodies.windows(2).enumerate() {
        let (prev, next) = (&pair[0], &pair[1]);
        assert_eq!(
            prev["instructions"],
            next["instructions"],
            "call {n} -> {}: the instructions changed, invalidating the whole prefix",
            n + 1
        );
        assert_eq!(
            prev["tools"],
            next["tools"],
            "call {n} -> {}: the tool list changed, invalidating the whole prefix",
            n + 1
        );
        let a = prev["input"].as_array().unwrap();
        let b = next["input"].as_array().unwrap();
        for (i, old) in a.iter().enumerate() {
            assert_eq!(
                b.get(i),
                Some(old),
                "call {n} -> {}: input item {i} of {} was rewritten instead of appended to",
                n + 1,
                a.len()
            );
        }
    }

    server.shutdown().await;
}

/// A workflow, over HTTP, from definition to a retried run.
///
/// The workflow surface had no end-to-end coverage at all, which is how a run
/// that could be neither answered nor interrupted, and step transcripts that
/// vanished on reload, all shipped. This drives the wire: define a graph, start
/// a run, watch it finish, read the projected graph, and retry a step.
///
/// Both steps deliberately declare no output schema. Such a step has no
/// `conclude` tool and ends its turn with plain text, which becomes its output —
/// so a run is two ordinary completions rather than two hand-built tool calls.
#[tokio::test]
async fn a_workflow_run_is_created_driven_and_retried_over_http() {
    let mock = MockLlmServer::builder().build().await;
    for _ in 0..4 {
        mock.queue_response("step done");
    }
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let server = start_server(tmp.path(), agent.link(), &mock.url()).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", server.addr);

    // A run resolves each step's preset and checks its model is still
    // configured, so both have to exist over the wire rather than be injected.
    // Pointing the provider at the mock is what `provider_at` already does, so
    // swapping the live registry changes nothing but the route.
    let res = client
        .put(format!("{base}/api/config"))
        .json(&serde_json::json!({
            "providers": [{
                "name": "p", "kind": "anthropic",
                "baseUrl": mock.url(), "apiKey": "test-key"
            }],
            "models": [{"alias": "mock", "provider": "p", "modelId": "m"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status().as_u16(),
        200,
        "configure the model a step runs"
    );

    let res = client
        .post(format!("{base}/api/agents"))
        .json(&serde_json::json!({"name": "wf-step", "model": "mock"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201, "the preset both steps run as");

    let res = client
        .post(format!("{base}/api/workflows"))
        .json(&serde_json::json!({
            "name": "e2e-flow",
            "start": "triage",
            "steps": [
                {
                    "name": "triage", "agent": "wf-step", "prompt": "Triage it.",
                    "transitions": [{"to": "fix"}]
                },
                {"name": "fix", "agent": "wf-step", "prompt": "Fix it."},
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201, "create the definition");

    // Creating the run is what starts it: there is no message to send.
    let res = client
        .post(format!("{base}/api/workflows/e2e-flow/runs"))
        .json(&serde_json::json!({"input": "the build is red", "vendor": "mock"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201, "start a run");
    let v: serde_json::Value = res.json().await.unwrap();
    let id = v["session"]["id"].as_str().unwrap().to_string();

    // The graph is the run's document, and it hangs off the session because a
    // run *is* one.
    let graph_url = format!("{base}/api/sessions/{id}/workflow");
    let graph = wait_for_run_status(&client, &graph_url, "Finished").await;
    let visited: Vec<&str> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| !n["runs"].as_array().unwrap().is_empty())
        .map(|n| n["step"].as_str().unwrap())
        .collect();
    assert_eq!(visited, vec!["triage", "fix"], "graph: {graph}");

    // Every node of the definition is present, reached or not, so a client draws
    // the whole graph and lights up what happened.
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(graph["edges"].as_array().unwrap().len(), 1);
    assert_eq!(
        graph["edges"][0]["traversals"].as_array().unwrap().len(),
        1,
        "the edge the run took records which execution took it"
    );

    // A step is addressable as an agent, which is where its transcript is.
    let step_agent = graph["nodes"][0]["runs"][0]["agentId"].as_str().unwrap();
    let res = client
        .get(format!("{base}/api/sessions/{id}/agents/{step_agent}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status().as_u16(),
        200,
        "a step's own page reads through the agent API"
    );
    let page = messages_page(&client, &server.addr, &id, step_agent).await;
    assert!(
        !page_messages(&page).is_empty(),
        "the step's transcript is what its page shows: {page}"
    );

    // Retrying appends an attempt rather than replacing one.
    let res = client
        .post(format!("{base}/api/sessions/{id}/workflow/retry"))
        .json(&serde_json::json!({"stepIndex": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 202, "retry one execution");
    let graph = wait_for_run_status(&client, &graph_url, "Finished").await;
    let fix = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["step"] == serde_json::json!("fix"))
        .unwrap();
    assert_eq!(
        fix["runs"].as_array().unwrap().len(),
        2,
        "the retry appends, so the earlier attempt stays readable: {fix}"
    );
    assert_eq!(fix["runs"][1]["attempt"], serde_json::json!(2));

    server.shutdown().await;
}

/// Poll a run's graph until its status is `want` (10s cap).
///
/// Asserts against the status rather than a step count: this suite is serial
/// against one long-lived server, so a baseline is the only safe comparison.
async fn wait_for_run_status(
    client: &reqwest::Client,
    graph_url: &str,
    want: &str,
) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for _ in 0..200 {
        let res = client.get(graph_url).send().await.unwrap();
        if res.status().is_success() {
            last = res.json().await.unwrap();
            if last["status"]["type"] == serde_json::json!(want) {
                return last;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run never reached {want}: {last}");
}
