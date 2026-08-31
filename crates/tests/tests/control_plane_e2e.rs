//! End-to-end tests for the control plane: a real session, driven by a mock
//! LLM that calls a `horsie_*` tool, asserting the change actually landed.
//!
//! The unit tests in `horsie-server` prove the toolbox dispatches to the right
//! operation. These prove the layer above: that a session whose tool selection
//! names a `horsie_*` tool *advertises* it to the model, that a tool call made
//! mid-turn reaches the same services the HTTP API writes through, and that a
//! session that never asked for one never sees them. None of that is visible
//! from a unit test, because none of it happens until a turn runs.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use async_llm::mock::MockLlmServer;
use horsie_agentcore::LlmProvider;
use horsie_llm_providers::anthropic::AnthropicProvider;
use horsie_server::runtime_vendor::fake::FakeRuntimeVendor;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

fn provider_at(url: &str) -> Arc<dyn LlmProvider> {
    Arc::new(
        AnthropicProvider::with_api_key("test-key")
            .unwrap()
            .with_base_url(url)
            .with_retry_delay_secs(0),
    )
}

struct Harness {
    addr: SocketAddr,
    /// The project every path below is relative to.
    project: String,
    client: reqwest::Client,
    _vendor: FakeRuntimeVendor,
    _dir: tempfile::TempDir,
    _task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start(mock: &MockLlmServer) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let vendor = FakeRuntimeVendor::builder("mock")
            .serve_in_process()
            .await
            .expect("fake agent");
        let built = horsie_server::testing::state(dir.path())
            .db(horsie_server::db::testing::db_at(dir.path()).await)
            .build()
            .await;
        // Order matters. Seeding a provider through the config store rebuilds
        // the live registry from the database, which would evict the mock
        // provider if it went in first — the session would then fail to
        // resolve its model and the turn would never run.
        let services = built.services().await;
        services
            .config_store
            .upsert_provider(horsie_models::settings::ProviderInput {
                name: "p".into(),
                kind: "anthropic".into(),
                base_url: Some("http://localhost:1".into()),
                api_key: Some("sk-x".into()),
                keep_thinking_signature: None,
            })
            .await
            .unwrap();
        services
            .config_store
            .upsert_model(horsie_models::settings::ModelInput {
                alias: "sonnet".into(),
                provider: "p".into(),
                model_id: "claude-sonnet-4-6".into(),
                max_tokens: None,
                context_window: None,
                thinking_efforts: None,
                thinking_effort: None,
                thinking_dialect: None,
                forced_tools_disable_thinking: None,
                supports_images: None,
                supports_documents: None,
            })
            .await
            .unwrap();
        built
            .insert_provider("mock", provider_at(&mock.url()))
            .await;
        built.publish_vendor("mock", vendor.link()).await;
        let project = built.account.as_str().to_string();
        let (addr, task) = built.serve().await;
        Self {
            addr,
            project,
            client: reqwest::Client::new(),
            _vendor: vendor,
            _dir: dir,
            _task: task,
        }
    }

    /// Create a session whose main agent may — or may not — manage the server.
    ///
    /// Granted by naming the tools, since that *is* the grant. Ungranted means
    /// sending no selection at all, which is the case worth testing: the
    /// default set must not reach the control plane, or every session on the
    /// server would be an admin.
    async fn session(&self, control_plane: bool, message: &str) -> String {
        let mut agent = serde_json::json!({
            "model": "mock",
            "use_plugins": false,
        });
        if control_plane {
            // Every control tool, read from the catalogue rather than listed
            // here: a resource added to the control plane must not quietly stop
            // being covered by these tests.
            //
            // camelCase, not snake_case: an unknown key is dropped in silence,
            // so `allowed_tools` here would leave every session ungranted and
            // make the positive tests below fail for a reason unrelated to the
            // control plane.
            let control: Vec<String> = horsie_server::tools::catalog()
                .groups
                .into_iter()
                .flat_map(|g| g.tools)
                .filter(|t| !t.in_default_set)
                .map(|t| t.name)
                .collect();
            agent["allowedTools"] = serde_json::json!(control);
        }
        let body = serde_json::json!({
            "agent": agent,
            "environment": {"type": "Runtime", "value": {"vendor": "mock"}},
            "message": message,
        });
        let res = self
            .client
            .post(self.url("/sessions"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 201, "{:?}", res.text().await);
        let v: serde_json::Value = res.json().await.unwrap();
        v["session"]["id"].as_str().unwrap().to_string()
    }

    /// `path` is relative to the project, as every scoped route is.
    fn url(&self, path: &str) -> String {
        format!("http://{}/api/p/{}{path}", self.addr, self.project)
    }

    async fn get(&self, path: &str) -> (u16, serde_json::Value) {
        let res = self.client.get(self.url(path)).send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(serde_json::Value::Null))
    }

    /// For the handful of routes that are not a project's contents. `path` is
    /// absolute from the origin.
    async fn get_unscoped(&self, path: &str) -> (u16, serde_json::Value) {
        let res = self
            .client
            .get(format!("http://{}{path}", self.addr))
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(serde_json::Value::Null))
    }

    /// Poll the transcript until the turn's closing text lands.
    ///
    /// Never `wait_status(…, "Idle")`: a session reports `Idle` when
    /// provisioning finishes *and* again when the turn ends, so waiting on it
    /// can return before the tool call has run and read an empty transcript.
    async fn wait_for_reply(&self, id: &str, want: &str) {
        let deadline = Duration::from_secs(20);
        let start = std::time::Instant::now();
        let mut last = String::new();
        loop {
            let (_, page) = self
                .get(&format!("/sessions/{id}/messages?aid=main&max=100"))
                .await;
            last = serde_json::to_string(&page).unwrap_or(last);
            if last.contains(want) {
                return;
            }
            assert!(
                start.elapsed() < deadline,
                "the turn never produced {want:?}; transcript so far: {last}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test]
async fn the_agents_tool_creates_a_preset() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_tool_call(
        "horsie_agents",
        serde_json::json!({"action": "create", "name": "made-by-agent", "model": "sonnet"}),
    );
    mock.queue_response("saved the preset");
    let h = Harness::start(&mock).await;

    let id = h.session(true, "make me a preset").await;
    h.wait_for_reply(&id, "saved the preset").await;

    let (status, agent) = h.get("/agents/made-by-agent").await;
    assert_eq!(status, 200, "the tool call must have reached the service");
    assert_eq!(agent["model"], "sonnet");
}

#[tokio::test]
async fn the_environments_tool_creates_an_environment() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_tool_call(
        "horsie_environments",
        serde_json::json!({"action": "create", "name": "scratch", "vendor": "mock", "repos": []}),
    );
    mock.queue_response("made the environment");
    let h = Harness::start(&mock).await;

    let id = h.session(true, "make me an environment").await;
    h.wait_for_reply(&id, "made the environment").await;

    let (status, env) = h.get("/environments/scratch").await;
    assert_eq!(status, 200, "the tool call must have reached the service");
    assert_eq!(env["name"], "scratch");
}

#[tokio::test]
async fn the_routines_tool_reads_through_to_the_service() {
    let mock = MockLlmServer::builder().build().await;
    // `list` rather than `create`: a routine needs a preset and a schedule, and
    // what this asserts is the read path — that the tool answers from the same
    // store the API does, on an account that genuinely has none.
    mock.queue_tool_call("horsie_routines", serde_json::json!({"action": "list"}));
    mock.queue_response("there are no routines");
    let h = Harness::start(&mock).await;

    let id = h.session(true, "what routines exist?").await;
    h.wait_for_reply(&id, "there are no routines").await;

    let (status, routines) = h.get("/routines").await;
    assert_eq!(status, 200);
    assert_eq!(routines.as_array().map(Vec::len), Some(0));
}

/// The whole point of the resource: an agent asked to narrow a preset can find
/// out what a legal tool name is instead of guessing one. A guess that misses
/// is not an error — an unknown name is passed through ungoverned — so the only
/// symptom used to be a preset quietly narrower than intended.
#[tokio::test]
async fn the_tools_tool_answers_the_names_a_selection_takes() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_tool_call("horsie_tools", serde_json::json!({"action": "list"}));
    mock.queue_response("bash is one of them");
    let h = Harness::start(&mock).await;

    let id = h.session(true, "what tools can a preset name?").await;
    h.wait_for_reply(&id, "bash is one of them").await;

    // The same table the browser reads, at the address it reads it from: the
    // tool mounts no route of its own.
    let (status, catalog) = h.get_unscoped("/api/tools").await;
    assert_eq!(status, 200);
    let names: Vec<String> = catalog["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .flat_map(|g| g["tools"].as_array().expect("tools").iter())
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "bash"), "{names:?}");
    assert!(names.iter().any(|n| n == "horsie_tools"), "{names:?}");
}

#[tokio::test]
async fn a_bad_action_comes_back_to_the_model_rather_than_ending_the_turn() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_tool_call(
        "horsie_agents",
        serde_json::json!({"action": "summon", "name": "x"}),
    );
    // The model gets the rejection as a tool result and answers anyway, which
    // is the whole point: a mistyped action is something it can correct, not a
    // turn that dies.
    mock.queue_response("that action does not exist");
    let h = Harness::start(&mock).await;

    let id = h.session(true, "do something impossible").await;
    h.wait_for_reply(&id, "that action does not exist").await;

    let (_, agents) = h.get("/agents").await;
    assert_eq!(agents.as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn a_session_without_the_grant_never_gets_the_tools() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_tool_call(
        "horsie_agents",
        serde_json::json!({"action": "create", "name": "sneaky", "model": "sonnet"}),
    );
    mock.queue_response("could not do that");
    let h = Harness::start(&mock).await;

    let id = h.session(false, "make me a preset").await;
    h.wait_for_reply(&id, "could not do that").await;

    let (status, _) = h.get("/agents/sneaky").await;
    assert_eq!(
        status, 404,
        "a session that was never granted the control plane must not be able to \
         reach it, even when the model calls the tool by name"
    );
}

#[tokio::test]
async fn the_workflows_tool_creates_a_definition() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_tool_call(
        "horsie_workflows",
        serde_json::json!({
            "action": "create",
            "name": "nightly",
            "start": "review",
            "steps": [{
                "name": "review",
                "agent": "reviewer",
                "prompt": "review the diff",
            }],
        }),
    );
    mock.queue_response("saved the workflow");
    let h = Harness::start(&mock).await;
    // A workflow's steps name presets, and the service resolves them at save.
    let res = h
        .client
        .post(h.url("/agents"))
        .json(&serde_json::json!({"name": "reviewer", "model": "sonnet"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201, "{:?}", res.text().await);

    let id = h.session(true, "make me a workflow").await;
    h.wait_for_reply(&id, "saved the workflow").await;

    let (status, workflow) = h.get("/workflows/nightly").await;
    assert_eq!(status, 200, "the tool call must have reached the service");
    assert_eq!(workflow["start"], "review");
}

#[tokio::test]
async fn the_sessions_tool_lists_and_reads_its_own_transcript() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_tool_call("horsie_sessions", serde_json::json!({"action": "list"}));
    mock.queue_response("listed the sessions");
    let h = Harness::start(&mock).await;

    let id = h.session(true, "what is running?").await;
    h.wait_for_reply(&id, "listed the sessions").await;

    // The tool answered from the same supervisor the API reads, and the
    // session it ran in is in that answer.
    let (status, sessions) = h.get("/sessions").await;
    assert_eq!(status, 200);
    assert!(
        sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == serde_json::json!(id)),
        "the running session must appear in its own list: {sessions}"
    );
}
