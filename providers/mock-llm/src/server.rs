use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
    routing::{get, post},
};
use futures::{StreamExt, stream::BoxStream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, convert::Infallible, net::SocketAddr, sync::Arc};
use tokio::{net::TcpListener, sync::Notify};

/// A queued response variant.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MockResponse {
    Text {
        content: String,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    /// `status` is used as the HTTP status code when returned as a plain JSON response,
    /// and as an SSE StreamError when returned in streaming mode.
    Error {
        status: u16,
        message: String,
    },
    TextStream {
        chunks: Vec<String>,
    },
    ToolCallStream {
        name: String,
        id: String,
        input: serde_json::Value,
    },
    Thinking {
        text: String,
        signature: String,
    },
}

struct QueueEntry {
    response: MockResponse,
    reached: Option<Arc<Notify>>,
    gate: Option<Arc<Notify>>,
}

impl QueueEntry {
    fn immediate(response: MockResponse) -> Self {
        Self {
            response,
            reached: None,
            gate: None,
        }
    }
}

pub struct BlockHandle {
    gate: Arc<Notify>,
    reached: Arc<Notify>,
}

impl BlockHandle {
    pub async fn wait_until_received(&self) {
        self.reached.notified().await;
    }
    pub fn release(&self) {
        self.gate.notify_one();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Scenario {
    #[serde(default)]
    pub description: String,
    pub responses: Vec<MockResponse>,
}

struct ScenarioState {
    responses: Vec<MockResponse>,
    cursor: usize,
}

impl ScenarioState {
    fn from_scenario(s: &Scenario) -> Self {
        Self {
            responses: s.responses.clone(),
            cursor: 0,
        }
    }
    fn next_response(&mut self) -> Option<MockResponse> {
        let resp = self.responses.get(self.cursor)?.clone();
        self.cursor += 1;
        Some(resp)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScenarioConfig {
    pub scenarios: HashMap<String, Scenario>,
}

#[derive(Default)]
struct MockState {
    queue: Mutex<Vec<QueueEntry>>,
    scenarios: Mutex<HashMap<String, Scenario>>,
    session_bindings: Mutex<HashMap<String, String>>,
    session_states: Mutex<HashMap<String, ScenarioState>>,
}

impl MockState {
    fn dequeue_entry(&self) -> Option<QueueEntry> {
        let mut q = self.queue.lock();
        (!q.is_empty()).then(|| q.remove(0))
    }
}

#[derive(Default)]
pub struct MockLlmServerBuilder {
    responses: Vec<MockResponse>,
    scenarios: HashMap<String, Scenario>,
    bind_all: bool,
    port: Option<u16>,
}

impl MockLlmServerBuilder {
    #[must_use]
    pub fn response(mut self, text: impl Into<String>) -> Self {
        self.responses.push(MockResponse::Text {
            content: text.into(),
        });
        self
    }
    #[must_use]
    pub fn tool_call(mut self, name: impl Into<String>, input: serde_json::Value) -> Self {
        self.responses.push(MockResponse::ToolCall {
            name: name.into(),
            input,
        });
        self
    }
    #[must_use]
    pub fn error(mut self, status: u16, message: impl Into<String>) -> Self {
        self.responses.push(MockResponse::Error {
            status,
            message: message.into(),
        });
        self
    }
    #[must_use]
    pub fn response_stream(mut self, chunks: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.responses.push(MockResponse::TextStream {
            chunks: chunks.into_iter().map(Into::into).collect(),
        });
        self
    }
    #[must_use]
    pub fn tool_call_stream(mut self, name: impl Into<String>, input: serde_json::Value) -> Self {
        self.responses.push(MockResponse::ToolCallStream {
            name: name.into(),
            id: format!("toolu_{}", uuid::Uuid::new_v4()),
            input,
        });
        self
    }
    #[must_use]
    pub fn thinking(mut self, text: impl Into<String>, signature: impl Into<String>) -> Self {
        self.responses.push(MockResponse::Thinking {
            text: text.into(),
            signature: signature.into(),
        });
        self
    }
    #[must_use]
    pub fn with_scenarios(mut self, config: ScenarioConfig) -> Self {
        self.scenarios = config.scenarios;
        self
    }
    #[must_use]
    pub fn bind_all_interfaces(mut self) -> Self {
        self.bind_all = true;
        self
    }
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }
    pub async fn build(self) -> MockLlmServer {
        let queue = self
            .responses
            .into_iter()
            .map(QueueEntry::immediate)
            .collect();
        let state = Arc::new(MockState {
            queue: Mutex::new(queue),
            scenarios: Mutex::new(self.scenarios),
            session_bindings: Mutex::new(HashMap::new()),
            session_states: Mutex::new(HashMap::new()),
        });
        let app = Router::new()
            .route("/v1/messages", post(handle_messages))
            .route("/queue", post(handle_queue))
            .route("/scenarios/load", post(handle_load_scenarios))
            .route("/scenarios", get(handle_list_scenarios))
            .route(
                "/scenarios/:name/register/:session_id",
                post(handle_register_session),
            )
            .with_state(state.clone());
        let port = self.port.unwrap_or(0);
        let bind = if self.bind_all {
            format!("0.0.0.0:{port}")
        } else {
            format!("127.0.0.1:{port}")
        };
        let listener = TcpListener::bind(&bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockLlmServer {
            addr,
            _handle: handle,
            state,
        }
    }
}

pub struct MockLlmServer {
    addr: SocketAddr,
    _handle: tokio::task::JoinHandle<()>,
    state: Arc<MockState>,
}

impl MockLlmServer {
    #[must_use]
    pub fn builder() -> MockLlmServerBuilder {
        MockLlmServerBuilder::default()
    }
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
    pub fn queued_count(&self) -> usize {
        self.state.queue.lock().len()
    }
    pub fn queue_response(&self, text: impl Into<String>) {
        self.state
            .queue
            .lock()
            .push(QueueEntry::immediate(MockResponse::Text {
                content: text.into(),
            }));
    }
    pub fn queue_tool_call(&self, name: impl Into<String>, input: serde_json::Value) {
        self.state
            .queue
            .lock()
            .push(QueueEntry::immediate(MockResponse::ToolCall {
                name: name.into(),
                input,
            }));
    }
    pub fn blocking_response(&self, text: impl Into<String>) -> BlockHandle {
        let gate = Arc::new(Notify::new());
        let reached = Arc::new(Notify::new());
        self.state.queue.lock().push(QueueEntry {
            response: MockResponse::Text {
                content: text.into(),
            },
            reached: Some(Arc::clone(&reached)),
            gate: Some(Arc::clone(&gate)),
        });
        BlockHandle { gate, reached }
    }
    pub fn load_scenarios(&self, config: ScenarioConfig) {
        self.state.scenarios.lock().extend(config.scenarios);
    }
    pub fn register_session(
        &self,
        session_id: impl Into<String>,
        scenario_name: impl Into<String>,
    ) {
        let session_id = session_id.into();
        let scenario_name = scenario_name.into();
        let scenarios = self.state.scenarios.lock();
        if let Some(scenario) = scenarios.get(&scenario_name) {
            let state = ScenarioState::from_scenario(scenario);
            drop(scenarios);
            self.state
                .session_states
                .lock()
                .insert(session_id.clone(), state);
            self.state
                .session_bindings
                .lock()
                .insert(session_id, scenario_name);
        }
    }
}

// ── internal request/response types ──────────────────────────────────────────

#[derive(Deserialize)]
struct MessagesRequest {
    #[serde(default)]
    stream: Option<bool>,
}

#[derive(Deserialize)]
struct QueueRequest {
    #[serde(rename = "type")]
    response_type: String,
    content: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct StatusResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize)]
struct ScenariosListResponse {
    scenarios: Vec<String>,
}

enum ResponseKind {
    Json(axum::Json<serde_json::Value>),
    Sse(Sse<BoxStream<'static, Result<Event, Infallible>>>),
    HttpError(StatusCode, axum::Json<serde_json::Value>),
}

impl axum::response::IntoResponse for ResponseKind {
    fn into_response(self) -> axum::response::Response {
        match self {
            ResponseKind::Json(j) => j.into_response(),
            ResponseKind::Sse(s) => s.into_response(),
            ResponseKind::HttpError(status, body) => (status, body).into_response(),
        }
    }
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn handle_messages(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(req): Json<MessagesRequest>,
) -> ResponseKind {
    let entry = if let Some(sid) = headers.get("X-Session-Id").and_then(|v| v.to_str().ok()) {
        let mut ss = state.session_states.lock();
        if let Some(scenario) = ss.get_mut(sid) {
            scenario.next_response().map(QueueEntry::immediate)
        } else {
            state.dequeue_entry()
        }
    } else {
        state.dequeue_entry()
    };

    if let Some(e) = &entry {
        if let Some(r) = &e.reached {
            r.notify_one();
        }
        if let Some(g) = &e.gate {
            g.notified().await;
        }
    }

    let response = entry.map(|e| e.response);
    let is_stream = req.stream.unwrap_or(false);

    match response {
        Some(MockResponse::TextStream { chunks }) => sse_from_pairs(text_stream_sse(&chunks)),
        Some(MockResponse::ToolCallStream { name, id, input }) => {
            sse_from_pairs(tool_call_stream_sse(&name, &id, &input))
        }
        other => {
            let resp = other;
            if is_stream {
                let msg_id = format!("msg_{}", uuid::Uuid::new_v4());
                let tool_id = format!("toolu_{}", uuid::Uuid::new_v4());
                let pairs = match resp {
                    Some(MockResponse::Text { content }) => text_sse(&msg_id, &content),
                    Some(MockResponse::ToolCall { name, input }) => {
                        tool_sse(&msg_id, &tool_id, &name, &input)
                    }
                    Some(MockResponse::Thinking { text, signature }) => {
                        thinking_sse(&msg_id, &text, &signature)
                    }
                    // Error in stream mode: emit a StreamError-compatible SSE event so
                    // async-anthropic parses it as AnthropicError::StreamError.
                    Some(MockResponse::Error { message, .. }) => vec![(
                        "error".into(),
                        // Top-level "type" + "message" matches async-anthropic's StreamError shape.
                        serde_json::json!({"type": "overloaded_error", "message": message})
                            .to_string(),
                    )],
                    None => text_sse(&msg_id, "No mock response queued"),
                    Some(MockResponse::TextStream { .. } | MockResponse::ToolCallStream { .. }) => {
                        unreachable!()
                    }
                };
                sse_from_pairs(pairs)
            } else {
                match resp {
                    Some(MockResponse::Text { content }) => {
                        ResponseKind::Json(axum::Json(text_json(&content)))
                    }
                    Some(MockResponse::ToolCall { name, input }) => {
                        ResponseKind::Json(axum::Json(tool_json(&name, &input)))
                    }
                    Some(MockResponse::Thinking { text, signature }) => {
                        ResponseKind::Json(axum::Json(thinking_json(&text, &signature)))
                    }
                    Some(MockResponse::Error { status, message }) => {
                        let code = StatusCode::from_u16(status)
                            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                        ResponseKind::HttpError(code, axum::Json(error_json(&message)))
                    }
                    None => ResponseKind::Json(axum::Json(text_json("No mock response queued"))),
                    Some(MockResponse::TextStream { .. } | MockResponse::ToolCallStream { .. }) => {
                        unreachable!()
                    }
                }
            }
        }
    }
}

async fn handle_queue(
    State(state): State<Arc<MockState>>,
    Json(req): Json<QueueRequest>,
) -> Json<StatusResponse> {
    let response = match req.response_type.as_str() {
        "text" => MockResponse::Text {
            content: req.content.unwrap_or_default(),
        },
        "tool_call" => MockResponse::ToolCall {
            name: req.tool_name.unwrap_or_default(),
            input: req.tool_input.unwrap_or_else(|| serde_json::json!({})),
        },
        _ => MockResponse::Text {
            content: "Unknown type".into(),
        },
    };
    state.queue.lock().push(QueueEntry::immediate(response));
    Json(StatusResponse {
        status: "queued".into(),
        message: None,
    })
}

async fn handle_load_scenarios(
    State(state): State<Arc<MockState>>,
    Json(config): Json<ScenarioConfig>,
) -> Json<StatusResponse> {
    let count = config.scenarios.len();
    state.scenarios.lock().extend(config.scenarios);
    Json(StatusResponse {
        status: "loaded".into(),
        message: Some(format!("{count} scenarios loaded")),
    })
}

async fn handle_list_scenarios(State(state): State<Arc<MockState>>) -> Json<ScenariosListResponse> {
    let scenarios = state.scenarios.lock();
    Json(ScenariosListResponse {
        scenarios: scenarios.keys().cloned().collect(),
    })
}

async fn handle_register_session(
    State(state): State<Arc<MockState>>,
    Path((scenario_name, session_id)): Path<(String, String)>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<StatusResponse>)> {
    let scenarios = state.scenarios.lock();
    if let Some(scenario) = scenarios.get(&scenario_name) {
        let scenario_state = ScenarioState::from_scenario(scenario);
        drop(scenarios);
        state
            .session_states
            .lock()
            .insert(session_id.clone(), scenario_state);
        state
            .session_bindings
            .lock()
            .insert(session_id.clone(), scenario_name.clone());
        Ok(Json(StatusResponse {
            status: "registered".into(),
            message: Some(format!(
                "Session {session_id} bound to scenario {scenario_name}"
            )),
        }))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(StatusResponse {
                status: "error".into(),
                message: Some(format!("Scenario '{scenario_name}' not found")),
            }),
        ))
    }
}

// ── SSE helpers ───────────────────────────────────────────────────────────────

fn sse_from_pairs(pairs: Vec<(String, String)>) -> ResponseKind {
    let events: Vec<Result<Event, Infallible>> = pairs
        .into_iter()
        .map(|(t, d)| Ok(Event::default().event(t).data(d)))
        .collect();
    ResponseKind::Sse(Sse::new(futures::stream::iter(events).boxed()))
}

fn text_sse(msg_id: &str, text: &str) -> Vec<(String, String)> {
    let tokens = u32::try_from(text.len() / 4).unwrap_or(u32::MAX);
    vec![
        (
            "message_start".into(),
            serde_json::json!({"type":"message_start","message":{"id":msg_id,"type":"message","role":"assistant","content":[],"model":"mock-model","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1}}}).to_string(),
        ),
        (
            "content_block_start".into(),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}).to_string(),
        ),
        (
            "content_block_delta".into(),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}).to_string(),
        ),
        (
            "content_block_stop".into(),
            serde_json::json!({"type":"content_block_stop","index":0}).to_string(),
        ),
        (
            "message_delta".into(),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":tokens}}).to_string(),
        ),
        (
            "message_stop".into(),
            serde_json::json!({"type":"message_stop"}).to_string(),
        ),
    ]
}

fn tool_sse(
    msg_id: &str,
    tool_id: &str,
    name: &str,
    input: &serde_json::Value,
) -> Vec<(String, String)> {
    let input_str = serde_json::to_string(input).unwrap_or_default();
    vec![
        (
            "message_start".into(),
            serde_json::json!({"type":"message_start","message":{"id":msg_id,"type":"message","role":"assistant","content":[],"model":"mock-model","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1}}}).to_string(),
        ),
        (
            "content_block_start".into(),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":tool_id,"name":name,"input":{}}}).to_string(),
        ),
        (
            "content_block_delta".into(),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input_str}}).to_string(),
        ),
        (
            "content_block_stop".into(),
            serde_json::json!({"type":"content_block_stop","index":0}).to_string(),
        ),
        (
            "message_delta".into(),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":20}}).to_string(),
        ),
        (
            "message_stop".into(),
            serde_json::json!({"type":"message_stop"}).to_string(),
        ),
    ]
}

fn thinking_sse(msg_id: &str, text: &str, signature: &str) -> Vec<(String, String)> {
    vec![
        (
            "message_start".into(),
            serde_json::json!({"type":"message_start","message":{"id":msg_id,"type":"message","role":"assistant","content":[],"model":"mock-model","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1}}}).to_string(),
        ),
        (
            "content_block_start".into(),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}).to_string(),
        ),
        (
            "content_block_delta".into(),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":text}}).to_string(),
        ),
        (
            "content_block_delta".into(),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":signature}}).to_string(),
        ),
        (
            "content_block_stop".into(),
            serde_json::json!({"type":"content_block_stop","index":0}).to_string(),
        ),
        (
            "message_delta".into(),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}).to_string(),
        ),
        (
            "message_stop".into(),
            serde_json::json!({"type":"message_stop"}).to_string(),
        ),
    ]
}

fn text_stream_sse(chunks: &[String]) -> Vec<(String, String)> {
    let msg_id = format!("msg_{}", uuid::Uuid::new_v4());
    let mut events = vec![
        (
            "message_start".into(),
            serde_json::json!({"type":"message_start","message":{"id":msg_id,"type":"message","role":"assistant","content":[],"model":"mock-model","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}).to_string(),
        ),
        (
            "content_block_start".into(),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}).to_string(),
        ),
    ];
    for chunk in chunks {
        events.push((
            "content_block_delta".into(),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":chunk}}).to_string(),
        ));
    }
    events.push((
        "content_block_stop".into(),
        serde_json::json!({"type":"content_block_stop","index":0}).to_string(),
    ));
    events.push((
        "message_delta".into(),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":chunks.len()}}).to_string(),
    ));
    events.push((
        "message_stop".into(),
        serde_json::json!({"type":"message_stop"}).to_string(),
    ));
    events
}

fn tool_call_stream_sse(name: &str, id: &str, input: &serde_json::Value) -> Vec<(String, String)> {
    let msg_id = format!("msg_{}", uuid::Uuid::new_v4());
    let input_str = input.to_string();
    let fragments: Vec<String> = input_str
        .as_bytes()
        .chunks(10)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect();
    let mut events = vec![
        (
            "message_start".into(),
            serde_json::json!({"type":"message_start","message":{"id":msg_id,"type":"message","role":"assistant","content":[],"model":"mock-model","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}).to_string(),
        ),
        (
            "content_block_start".into(),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}).to_string(),
        ),
    ];
    for frag in &fragments {
        events.push((
            "content_block_delta".into(),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":frag}}).to_string(),
        ));
    }
    events.push((
        "content_block_stop".into(),
        serde_json::json!({"type":"content_block_stop","index":0}).to_string(),
    ));
    events.push((
        "message_delta".into(),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":20}}).to_string(),
    ));
    events.push((
        "message_stop".into(),
        serde_json::json!({"type":"message_stop"}).to_string(),
    ));
    events
}

// ── JSON response helpers ─────────────────────────────────────────────────────

fn text_json(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "id": format!("msg_{}", uuid::Uuid::new_v4()),
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "model": "mock-model",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": text.len() / 4}
    })
}

fn tool_json(name: &str, input: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "id": format!("msg_{}", uuid::Uuid::new_v4()),
        "role": "assistant",
        "content": [{"type": "tool_use", "id": format!("toolu_{}", uuid::Uuid::new_v4()), "name": name, "input": input}],
        "model": "mock-model",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    })
}

fn thinking_json(text: &str, signature: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "id": format!("msg_{}", uuid::Uuid::new_v4()),
        "role": "assistant",
        "content": [{"type": "thinking", "thinking": text, "signature": signature}],
        "model": "mock-model",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

fn error_json(message: &str) -> serde_json::Value {
    serde_json::json!({"type": "error", "error": {"type": "api_error", "message": message}})
}
