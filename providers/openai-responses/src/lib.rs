//! An `LlmProvider` for the OpenAI **Responses** API.
//!
//! Two credentials reach the same wire. An API key targets
//! `{base_url}/responses` on api.openai.com (or any Responses-compatible
//! deployment); a ChatGPT-plan OAuth token targets
//! `https://chatgpt.com/backend-api/codex/responses`, which is the only way to
//! spend a Codex subscription. Everything between those two facts — request
//! building, stream parsing, retries — is credential-blind.
//!
//! Why not a flag on `horsie-openai`: the Responses wire is a different shape,
//! not a dialect. History is a flat item list, tool definitions are flat, and
//! thinking is replayed rather than dropped. See [`wire`].

pub mod chatgpt;
pub mod wire;

use async_trait::async_trait;
use chatgpt::{CHATGPT_RESPONSES_URL, ChatGptTokens, ORIGINATOR};
use futures_util::StreamExt;
use horsie_agentcore::{
    AgentEvent, CompletionRequest, CompletionResponse, ContentBlockStopEvent, ContentPart,
    EventSink, LlmError, LlmProvider, Secret, StopReason, TextBlockStartEvent, TextChunkEvent,
    TextPart, ThinkingBlockStartEvent, ThinkingChunkEvent, ThinkingDialect, ThinkingPart,
    ToolCallInputDeltaEvent, ToolCallPart, ToolCallStartEvent, ToolChoice, Usage,
};
use reqwest_eventsource::{Event, EventSource};
use std::{collections::BTreeMap, env, sync::Arc, time::Duration};
use wire::{FunctionTool, ReasoningControl, ReasoningRef, ResponsesRequest, to_input_items};

pub const DEFAULT_MODEL: &str = "gpt-5";
pub const DEFAULT_MAX_TOKENS: u32 = 32_768;
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const MAX_STREAM_RETRIES: u32 = 6;
const BACKOFF_BASE_SECS: u64 = 5;
/// Bounds TCP + TLS setup. A peer that never completes a handshake is dead, not slow.
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// Bounds *idle* time between reads, not the total call — a reasoning model can
/// legitimately take minutes before its first visible token.
const DEFAULT_READ_TIMEOUT_SECS: u64 = 180;

#[must_use]
pub fn env_base_url() -> Option<String> {
    env::var("OPENAI_BASE_URL").ok().filter(|s| !s.is_empty())
}

/// Parse a streamed tool call's argument JSON.
///
/// An empty string means "no arguments" — the API sends that for zero-parameter
/// tools — and becomes `{}`. Anything else that fails to parse is a malformed
/// response, and is reported as such rather than silently replaced: dispatching
/// a tool with fabricated arguments turns a provider failure into a confusing
/// tool failure.
fn parse_tool_input(raw: &str, tool: &str) -> Result<serde_json::Value, LlmError> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|e| LlmError::ApiError {
        status: 502,
        message: format!(
            "tool call '{tool}' had unparseable input JSON ({e}); \
             {} byte(s) received, likely a truncated stream",
            raw.len()
        ),
    })
}

fn build_http(read_timeout_secs: u64) -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .read_timeout(Duration::from_secs(read_timeout_secs))
        // chatgpt.com sits behind Cloudflare, which hands out clearance cookies
        // and expects them back. Codex keeps a jar for exactly this reason.
        .cookie_store(true)
        .build()
        .map_err(|e| LlmError::Network(Box::new(e)))
}

fn io_err(msg: impl std::fmt::Display) -> LlmError {
    LlmError::Network(Box::new(std::io::Error::other(msg.to_string())))
}

/// Longest advised wait this provider will actually sit through.
///
/// A ChatGPT plan's 429 is not the API's per-minute blip: it means the
/// subscription's window is spent, and the reset is measured in hours. Sleeping
/// through that would hold a turn open for the rest of the day, so anything
/// longer than this is returned to the caller with its reset time attached
/// instead of being retried.
const MAX_ADVISED_WAIT_SECS: u64 = 60;

/// Map an HTTP status onto a classified error. Getting this wrong is not
/// cosmetic: a 429 misfiled as `Network` is never retried.
///
/// `advised` is the server's own `Retry-After`, when it sent one.
fn classify_status(status: u16, body: &str, advised: Option<Duration>) -> LlmError {
    match status {
        429 => LlmError::RateLimit {
            retry_after: advised,
        },
        500 | 502 | 503 | 504 | 529 => LlmError::Overloaded,
        _ => LlmError::ApiError {
            status,
            message: body.to_string(),
        },
    }
}

/// The server's advised wait, from whichever header it used.
///
/// `Retry-After` is the standard one and may be either seconds or an HTTP
/// date; the ChatGPT backend also sends `x-ratelimit-reset-requests` in
/// seconds. Only the numeric forms are read — a date form is rare here and a
/// missing hint is not an error, just a fall back to our own backoff.
fn advised_wait(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    for name in ["retry-after", "x-ratelimit-reset-requests"] {
        if let Some(secs) = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().trim_end_matches('s').parse::<f64>().ok())
            && secs >= 0.0
        {
            return Some(Duration::from_secs_f64(secs));
        }
    }
    None
}

/// Whether waiting could plausibly help.
///
/// A rate limit whose own reset is further out than we are willing to wait is
/// terminal for this turn: retrying it just burns attempts against a window
/// that will not reopen in time, and on a subscription those attempts are the
/// scarce resource.
fn is_retryable(e: &LlmError) -> bool {
    match e {
        LlmError::RateLimit { retry_after } => {
            retry_after.is_none_or(|d| d.as_secs() <= MAX_ADVISED_WAIT_SECS)
        }
        LlmError::Overloaded => true,
        LlmError::ApiError { .. } | LlmError::Network(_) | LlmError::EventSink(_) => false,
    }
}

/// A key that is the same for every turn of one conversation and different
/// across conversations.
///
/// The first message's id is exactly that: assigned once when the conversation
/// starts and replayed in every later request, while the provider is handed no
/// session id of its own. `None` for an empty history, where there is no prefix
/// worth caching anyway.
fn conversation_cache_key(messages: &[horsie_agentcore::Message]) -> Option<String> {
    messages.first().map(|m| format!("horsie-{}", m.id))
}

/// How a request authenticates, and therefore where it goes.
pub enum Credential {
    /// A platform API key. Targets `{base_url}/responses`.
    ApiKey(Secret),
    /// A ChatGPT subscription. Targets the Codex backend, and carries an
    /// account id alongside the bearer token.
    ChatGpt(Arc<ChatGptTokens>),
    /// No credential at all — a local Responses-compatible server.
    None,
}

/// Seconds since the epoch. Token expiry is absolute, so the clock has to be
/// read at the moment of use rather than captured at construction.
fn now_secs() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

pub struct ResponsesProvider {
    http: reqwest::Client,
    model: String,
    credential: Credential,
    base_url: String,
    max_tokens: Option<u32>,
    /// Whether this model takes an effort control. `OpenAiEffort` sends
    /// `reasoning.effort`; everything else sends no reasoning control. The
    /// dialect vocabulary is shared with the chat wire on purpose — the
    /// canonical effort is the same value, only its encoding differs.
    thinking_dialect: ThinkingDialect,
    retry_base_secs: u64,
    read_timeout_secs: u64,
}

impl ResponsesProvider {
    fn build(credential: Credential) -> Result<Self, LlmError> {
        Ok(Self {
            http: build_http(DEFAULT_READ_TIMEOUT_SECS)?,
            model: DEFAULT_MODEL.to_string(),
            credential,
            base_url: env_base_url().unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            max_tokens: None,
            thinking_dialect: ThinkingDialect::NoControl,
            retry_base_secs: BACKOFF_BASE_SECS,
            read_timeout_secs: DEFAULT_READ_TIMEOUT_SECS,
        })
    }

    /// Reads `OPENAI_API_KEY` if set. An absent key is not an error — a local
    /// Responses-compatible server needs none.
    pub fn new() -> Result<Self, LlmError> {
        let key = env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Secret::from);
        Self::build(key.map_or(Credential::None, Credential::ApiKey))
    }

    /// A provider that spends a ChatGPT subscription.
    ///
    /// The base URL is the Codex backend rather than `api.openai.com`: a plan
    /// is not reachable through the platform API. `with_base_url` still
    /// overrides it, which is how the tests point this at a mock.
    pub fn with_chatgpt(tokens: Arc<ChatGptTokens>) -> Result<Self, LlmError> {
        let mut p = Self::build(Credential::ChatGpt(tokens))?;
        p.base_url = CHATGPT_RESPONSES_URL
            .trim_end_matches("/responses")
            .to_string();
        Ok(p)
    }

    pub fn with_api_key(key: impl Into<Secret>) -> Result<Self, LlmError> {
        Self::build(Credential::ApiKey(key.into()))
    }

    #[must_use]
    pub fn with_model(mut self, m: impl Into<String>) -> Self {
        self.model = m.into();
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, u: impl Into<String>) -> Self {
        self.base_url = u.into().trim_end_matches('/').to_string();
        self
    }

    #[must_use]
    pub fn with_max_tokens(mut self, t: Option<u32>) -> Self {
        self.max_tokens = t;
        self
    }

    #[must_use]
    pub fn with_thinking_dialect(mut self, dialect: ThinkingDialect) -> Self {
        self.thinking_dialect = dialect;
        self
    }

    #[must_use]
    pub fn with_retry_delay_secs(mut self, secs: u64) -> Self {
        self.retry_base_secs = secs;
        self
    }

    /// Bound how long the client waits on a silent peer. Idle time between
    /// reads, not total call duration, so a long generation is unaffected.
    #[must_use]
    pub fn with_read_timeout_secs(mut self, secs: u64) -> Self {
        self.read_timeout_secs = secs;
        if let Ok(http) = build_http(secs) {
            self.http = http;
        }
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> ResponsesRequest {
        let tools: Vec<FunctionTool> = request
            .tools
            .iter()
            .map(|t| FunctionTool {
                kind: "function",
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            })
            .collect();

        let tool_choice = if tools.is_empty() {
            None
        } else {
            match &request.tool_choice {
                ToolChoice::Auto => None,
                ToolChoice::Any => Some(serde_json::json!("required")),
                // Flat, like the tool definitions — the chat wire's nested
                // `{function:{name}}` is rejected here.
                ToolChoice::Required(name) => Some(serde_json::json!({
                    "type": "function",
                    "name": name
                })),
            }
        };

        let reasoning = match (self.thinking_dialect, request.thinking_effort) {
            (ThinkingDialect::OpenAiEffort, Some(e)) => Some(ReasoningControl {
                effort: e.as_str().to_string(),
                summary: "auto",
            }),
            _ => None,
        };

        ResponsesRequest {
            model: self.model.clone(),
            instructions: request.system.clone(),
            input: to_input_items(request.messages),
            tools,
            tool_choice,
            max_output_tokens: self
                .max_tokens
                .or(request.max_tokens)
                .or(Some(DEFAULT_MAX_TOKENS)),
            reasoning,
            store: false,
            stream: true,
            include: vec!["reasoning.encrypted_content"],
            prompt_cache_key: conversation_cache_key(request.messages),
        }
    }
}

/// One output item being streamed, keyed by its `output_index`.
///
/// The Responses API sends every item twice: `output_item.added` opens it and
/// `output_item.done` repeats it complete. Deltas exist to drive live UI. So
/// the final content is taken from `done` — authoritative and already
/// assembled — while the deltas only emit events.
#[derive(Debug, Default)]
struct ItemAcc {
    kind: String,
    tool_call_id: String,
    name: String,
}

struct StreamState {
    items: BTreeMap<u32, ItemAcc>,
    parts: Vec<ContentPart>,
    usage: Usage,
    emitted_anything: bool,
}

// `Usage` has no `Default` impl — it is fluorite-generated — so this is written
// out rather than derived.
impl Default for StreamState {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            parts: Vec::new(),
            usage: Usage::without_cache(0, 0),
            emitted_anything: false,
        }
    }
}

impl ResponsesProvider {
    /// Fold one SSE frame into `state`. Returns a stop reason once the response
    /// reaches a terminal event.
    async fn absorb_event(
        kind: &str,
        value: &serde_json::Value,
        state: &mut StreamState,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<Option<StopReason>, LlmError> {
        let index = u32::try_from(value["output_index"].as_u64().unwrap_or(0)).unwrap_or(0);

        match kind {
            "response.output_item.added" => {
                let item = &value["item"];
                let item_kind = item["type"].as_str().unwrap_or_default().to_string();
                match item_kind.as_str() {
                    "message" => {
                        events
                            .emit(AgentEvent::TextBlockStart(TextBlockStartEvent {
                                message_id: message_id.to_string(),
                                index,
                            }))
                            .await?;
                    }
                    "reasoning" => {
                        events
                            .emit(AgentEvent::ThinkingBlockStart(ThinkingBlockStartEvent {
                                message_id: message_id.to_string(),
                                index,
                            }))
                            .await?;
                    }
                    "function_call" => {
                        let tool_call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                        let name = item["name"].as_str().unwrap_or_default().to_string();
                        state.emitted_anything = true;
                        events
                            .emit(AgentEvent::ToolCallStart(ToolCallStartEvent {
                                message_id: message_id.to_string(),
                                index,
                                tool_call_id: tool_call_id.clone(),
                                name: name.clone(),
                            }))
                            .await?;
                        state.items.insert(
                            index,
                            ItemAcc {
                                kind: item_kind,
                                tool_call_id,
                                name,
                            },
                        );
                        return Ok(None);
                    }
                    _ => {}
                }
                state.items.insert(
                    index,
                    ItemAcc {
                        kind: item_kind,
                        ..ItemAcc::default()
                    },
                );
            }

            "response.output_text.delta" => {
                if let Some(d) = value["delta"].as_str()
                    && !d.is_empty()
                {
                    state.emitted_anything = true;
                    events
                        .emit(AgentEvent::TextChunk(TextChunkEvent {
                            message_id: message_id.to_string(),
                            index,
                            text: d.to_string(),
                        }))
                        .await?;
                }
            }

            // The visible summary. The real chain of thought only ever arrives
            // encrypted, on the item's `done` frame.
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = value["delta"].as_str()
                    && !d.is_empty()
                {
                    state.emitted_anything = true;
                    events
                        .emit(AgentEvent::ThinkingChunk(ThinkingChunkEvent {
                            message_id: message_id.to_string(),
                            index,
                            text: d.to_string(),
                        }))
                        .await?;
                }
            }

            "response.function_call_arguments.delta" => {
                if let Some(d) = value["delta"].as_str()
                    && !d.is_empty()
                    && let Some(acc) = state.items.get(&index)
                {
                    events
                        .emit(AgentEvent::ToolCallInputDelta(ToolCallInputDeltaEvent {
                            message_id: message_id.to_string(),
                            index,
                            tool_call_id: acc.tool_call_id.clone(),
                            delta: d.to_string(),
                        }))
                        .await?;
                }
            }

            "response.output_item.done" => {
                let item = &value["item"];
                let acc = state.items.remove(&index).unwrap_or_default();
                let item_kind = item["type"].as_str().unwrap_or(acc.kind.as_str());

                match item_kind {
                    "message" => {
                        let text: String = item["content"]
                            .as_array()
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p["text"].as_str())
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        if !text.is_empty() {
                            state.emitted_anything = true;
                            state.parts.push(ContentPart::Text(TextPart { text }));
                        }
                    }
                    "reasoning" => {
                        let summary: String = item["summary"]
                            .as_array()
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p["text"].as_str())
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        // Without the encrypted blob the item cannot be replayed,
                        // so the part is display-only and carries no signature.
                        let signature = item["encrypted_content"].as_str().map(|enc| {
                            ReasoningRef {
                                id: item["id"].as_str().unwrap_or_default().to_string(),
                                encrypted: enc.to_string(),
                            }
                            .to_signature()
                        });
                        if !summary.is_empty() || signature.is_some() {
                            state.parts.push(ContentPart::Thinking(ThinkingPart {
                                text: summary,
                                signature,
                            }));
                        }
                    }
                    "function_call" => {
                        let name = item["name"]
                            .as_str()
                            .unwrap_or(acc.name.as_str())
                            .to_string();
                        let raw = item["arguments"].as_str().unwrap_or_default();
                        let input = parse_tool_input(raw, &name)?;
                        state.parts.push(ContentPart::ToolCall(ToolCallPart {
                            id: item["call_id"]
                                .as_str()
                                .unwrap_or(acc.tool_call_id.as_str())
                                .to_string(),
                            name,
                            input,
                        }));
                    }
                    _ => {}
                }

                events
                    .emit(AgentEvent::ContentBlockStop(ContentBlockStopEvent {
                        message_id: message_id.to_string(),
                        index,
                    }))
                    .await?;
            }

            "response.completed" | "response.incomplete" => {
                let response = &value["response"];
                if let Some(u) = response.get("usage") {
                    state.usage.input_tokens =
                        u32::try_from(u["input_tokens"].as_u64().unwrap_or(0)).unwrap_or(0);
                    state.usage.output_tokens =
                        u32::try_from(u["output_tokens"].as_u64().unwrap_or(0)).unwrap_or(0);
                    // `input_tokens` already includes cached tokens, which is
                    // exactly what `Usage`'s contract requires — no adjustment.
                    state.usage.cache_read_tokens = u["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok());
                }

                let truncated = response["incomplete_details"]["reason"].as_str()
                    == Some("max_output_tokens")
                    || kind == "response.incomplete";
                let has_tool_call = state
                    .parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::ToolCall(_)));

                return Ok(Some(if truncated {
                    StopReason::MaxTokens
                } else if has_tool_call {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                }));
            }

            "response.failed" => {
                let message = value["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("response failed")
                    .to_string();
                return Err(LlmError::ApiError {
                    status: 502,
                    message,
                });
            }

            "error" => {
                let message = value["message"]
                    .as_str()
                    .unwrap_or("stream error")
                    .to_string();
                return Err(LlmError::ApiError {
                    status: 502,
                    message,
                });
            }

            _ => {}
        }

        Ok(None)
    }
}

#[async_trait]
impl LlmProvider for ResponsesProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        let body = self.build_body(&request);
        let mut last_error: Option<LlmError> = None;
        // A 401 buys exactly one forced refresh. More would spin against a
        // revoked login, and the operator needs to be told to sign in again
        // rather than have it retried at them.
        let mut refreshed_after_401 = false;
        // Set when the previous attempt failed for a reason a wait cannot fix.
        let mut skip_backoff = false;
        // The server's own advised wait, when it sent one. Preferred over our
        // backoff: it knows when the window reopens and we are guessing.
        let mut advised: Option<Duration> = None;

        'retry: for attempt in 0..=MAX_STREAM_RETRIES {
            if attempt > 0 && !skip_backoff {
                let delay = advised.take().unwrap_or_else(|| {
                    Duration::from_secs(self.retry_base_secs * 2u64.pow(attempt - 1))
                });
                tracing::warn!(attempt, delay_secs = delay.as_secs(), "Responses retry");
                tokio::time::sleep(delay).await;
            }
            skip_backoff = false;

            let mut req = self.http.post(self.endpoint()).json(&body);
            match &self.credential {
                Credential::ApiKey(k) => req = req.bearer_auth(k.expose()),
                Credential::ChatGpt(tokens) => {
                    req = req
                        .bearer_auth(tokens.access_token(now_secs()).await?)
                        .header("ChatGPT-Account-ID", tokens.account_id())
                        // Who we are. Never `codex_cli_rs`, and never an
                        // `x-oai-attestation` we are not entitled to mint.
                        .header("originator", ORIGINATOR);
                }
                Credential::None => {}
            }

            let mut state = StreamState::default();
            let mut stop_reason = StopReason::EndTurn;
            // A stream that just stops — the connection dropped mid-response —
            // must not be mistaken for a completed turn.
            let mut saw_terminal = false;
            let mut es = EventSource::new(req).map_err(io_err)?;

            while let Some(ev) = es.next().await {
                match ev {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(m)) => {
                        if m.data.trim() == "[DONE]" {
                            saw_terminal = true;
                            break;
                        }
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.data) else {
                            continue;
                        };
                        // The frame's own `type` is authoritative; the SSE event
                        // name is a convenience some deployments omit.
                        let kind = value["type"]
                            .as_str()
                            .unwrap_or(m.event.as_str())
                            .to_string();
                        match Self::absorb_event(&kind, &value, &mut state, message_id, events)
                            .await
                        {
                            Ok(Some(f)) => {
                                stop_reason = f;
                                saw_terminal = true;
                                break;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                es.close();
                                return Err(e);
                            }
                        }
                    }
                    Err(reqwest_eventsource::Error::StreamEnded) => break,
                    Err(reqwest_eventsource::Error::InvalidStatusCode(status, resp)) => {
                        let code = status.as_u16();
                        // Read before the body: `text()` consumes the response.
                        advised = advised_wait(resp.headers());
                        let body_text = resp.text().await.unwrap_or_default();
                        let err = classify_status(code, &body_text, advised);
                        es.close();
                        // A token can be revoked long before it expires, so an
                        // unexpired-looking credential can still be refused.
                        // Refresh once and try again immediately — waiting out a
                        // backoff would not make a stale token any fresher.
                        if code == 401
                            && !refreshed_after_401
                            && !state.emitted_anything
                            && let Credential::ChatGpt(tokens) = &self.credential
                        {
                            refreshed_after_401 = true;
                            skip_backoff = true;
                            tokens.refresh(now_secs()).await?;
                            continue 'retry;
                        }
                        // Only retry when nothing has been emitted — re-running a
                        // partially streamed turn would duplicate content the
                        // caller has already seen.
                        if is_retryable(&err) && !state.emitted_anything {
                            last_error = Some(err);
                            continue 'retry;
                        }
                        return Err(err);
                    }
                    Err(e) => {
                        es.close();
                        return Err(io_err(e));
                    }
                }
            }

            es.close();

            // A stream that ended without its terminal frame is a dropped
            // connection, not an answer. Returning what arrived would journal a
            // truncated assistant turn and present it to the user as success.
            if !saw_terminal {
                let err = last_error.take().unwrap_or_else(|| {
                    io_err(
                        "stream ended without a terminal frame (connection dropped mid-response)",
                    )
                });
                if is_retryable(&err) && !state.emitted_anything {
                    last_error = Some(err);
                    continue 'retry;
                }
                return Err(err);
            }

            return Ok(CompletionResponse {
                parts: state.parts,
                stop_reason,
                usage: state.usage,
            });
        }

        Err(last_error.unwrap_or_else(|| io_err("stream retries exhausted")))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_agentcore::EventSinkError;
    use horsie_mock_llm::MockLlmServer;
    use horsie_models::agent::{Message, Role};
    use std::sync::{Mutex, PoisonError};

    struct NullSink(Mutex<Vec<AgentEvent>>);

    impl NullSink {
        fn new() -> Self {
            Self(Mutex::new(Vec::new()))
        }
        fn events(&self) -> Vec<AgentEvent> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl EventSink for NullSink {
        async fn emit(&self, e: AgentEvent) -> Result<(), EventSinkError> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(e);
            Ok(())
        }
    }

    fn user(text: &str) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: "m1".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart {
                text: text.to_string(),
            })],
        }
    }

    fn request(messages: &[Message]) -> CompletionRequest<'_> {
        CompletionRequest {
            messages,
            system: None,
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_tokens: Some(64),
            thinking_effort: None,
        }
    }

    async fn provider_for(server: &MockLlmServer) -> ResponsesProvider {
        ResponsesProvider::with_api_key("test-key")
            .unwrap()
            .with_base_url(server.url())
            .with_model("mock-model")
            .with_retry_delay_secs(0)
    }

    #[tokio::test]
    async fn a_text_turn_returns_one_text_part() {
        let server = MockLlmServer::builder().build().await;
        server.queue_response("hello there");
        let p = provider_for(&server).await;
        let sink = NullSink::new();

        let history = vec![user("hi")];
        let res = p.complete(request(&history), "msg-1", &sink).await.unwrap();

        assert_eq!(res.stop_reason, StopReason::EndTurn);
        assert_eq!(res.parts.len(), 1);
        match &res.parts[0] {
            ContentPart::Text(t) => assert_eq!(t.text, "hello there"),
            other => panic!("expected text, got {other:?}"),
        }
        assert!(
            sink.events()
                .iter()
                .any(|e| matches!(e, AgentEvent::TextChunk(_))),
            "the turn must stream text chunks"
        );
    }

    #[tokio::test]
    async fn a_tool_call_returns_a_parsed_tool_call_part() {
        let server = MockLlmServer::builder().build().await;
        server.queue_tool_call("echo", serde_json::json!({ "value": 42 }));
        let p = provider_for(&server).await;
        let sink = NullSink::new();

        let history = vec![user("call it")];
        let res = p.complete(request(&history), "msg-1", &sink).await.unwrap();

        assert_eq!(res.stop_reason, StopReason::ToolUse);
        match &res.parts[0] {
            ContentPart::ToolCall(tc) => {
                assert_eq!(tc.name, "echo");
                assert_eq!(tc.input, serde_json::json!({ "value": 42 }));
                assert!(!tc.id.is_empty());
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    /// The point of the crate: reasoning comes back replayable.
    #[tokio::test]
    async fn a_reasoning_turn_carries_a_replayable_signature() {
        let server = MockLlmServer::builder().build().await;
        server.queue_reasoning("thinking about it", "the answer");
        let p = provider_for(&server).await;
        let sink = NullSink::new();

        let history = vec![user("hi")];
        let res = p.complete(request(&history), "msg-1", &sink).await.unwrap();

        let thinking = res
            .parts
            .iter()
            .find_map(|p| match p {
                ContentPart::Thinking(t) => Some(t),
                _ => None,
            })
            .expect("a thinking part");
        assert_eq!(thinking.text, "thinking about it");
        let sig = thinking.signature.as_deref().expect("a signature");
        let parsed = ReasoningRef::from_signature(sig).expect("a reasoning ref");
        assert!(!parsed.encrypted.is_empty());
    }

    #[tokio::test]
    async fn truncation_surfaces_as_max_tokens() {
        let server = MockLlmServer::builder().build().await;
        server.queue_truncated("cut off");
        let p = provider_for(&server).await;

        let history = vec![user("hi")];
        let res = p
            .complete(request(&history), "msg-1", &NullSink::new())
            .await
            .unwrap();

        assert_eq!(res.stop_reason, StopReason::MaxTokens);
    }

    #[tokio::test]
    async fn a_cut_stream_is_an_error_not_an_empty_success() {
        let server = MockLlmServer::builder().build().await;
        server.queue_cut_stream(["hel", "lo"], 1);
        let p = provider_for(&server).await;

        let history = vec![user("hi")];
        let err = p
            .complete(request(&history), "msg-1", &NullSink::new())
            .await
            .expect_err("a dropped stream must not read as success");

        assert!(matches!(err, LlmError::Network(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn usage_reports_cached_tokens_from_the_completed_frame() {
        let server = MockLlmServer::builder().build().await;
        server.queue_response("hi");
        let p = provider_for(&server).await;

        let history = vec![user("hi")];
        let res = p
            .complete(request(&history), "msg-1", &NullSink::new())
            .await
            .unwrap();

        assert_eq!(res.usage.input_tokens, 10);
        assert_eq!(res.usage.output_tokens, 5);
        assert_eq!(res.usage.cache_read_tokens, Some(4));
    }

    #[tokio::test]
    async fn a_rate_limit_is_classified_rather_than_read_as_a_network_error() {
        let server = MockLlmServer::builder().build().await;
        // One queued 429 does not test this: the provider retries, finds an
        // empty queue, and gets a normal completion. Queue enough for every
        // attempt.
        for _ in 0..12 {
            server.queue_error(429, "slow down");
        }
        let p = provider_for(&server).await;

        let history = vec![user("hi")];
        let err = p
            .complete(request(&history), "msg-1", &NullSink::new())
            .await
            .expect_err("429s must surface");

        assert!(matches!(err, LlmError::RateLimit { .. }), "got {err:?}");
    }

    /// A ChatGPT provider whose token is already spent must renew it and get on
    /// with the turn — an operator never sees this happen.
    #[tokio::test]
    async fn an_expired_chatgpt_credential_refreshes_before_the_turn() {
        use crate::chatgpt::{ChatGptTokens, StoredTokens, tests::RecordingStore};

        let server = MockLlmServer::builder().build().await;
        server.queue_response("hi there");
        let issuer = crate::chatgpt::tests::mock_issuer(200, false).await;
        let store = Arc::new(RecordingStore::default());
        let tokens = Arc::new(ChatGptTokens::new(
            StoredTokens {
                access: "stale".into(),
                refresh: "refresh-1".into(),
                expires_at: 0,
                account_id: "acct_1".into(),
            },
            store.clone(),
            issuer,
        ));

        let p = ResponsesProvider::with_chatgpt(tokens)
            .unwrap()
            .with_base_url(server.url())
            .with_model("mock-model")
            .with_retry_delay_secs(0);

        let history = vec![user("hi")];
        let res = p
            .complete(request(&history), "msg-1", &NullSink::new())
            .await
            .unwrap();

        assert_eq!(res.parts.len(), 1);
        assert_eq!(store.saved().len(), 1, "the refreshed token is persisted");
        assert_eq!(store.saved()[0].access, "access-2");
    }

    /// A token can be revoked while it still looks valid. One forced refresh,
    /// one retry, no backoff — waiting would not make a dead token live.
    #[tokio::test]
    async fn a_401_forces_one_refresh_and_retries() {
        use crate::chatgpt::{ChatGptTokens, StoredTokens, tests::RecordingStore};

        let server = MockLlmServer::builder().build().await;
        server.queue_error(401, "token revoked");
        server.queue_response("second time lucky");
        let issuer = crate::chatgpt::tests::mock_issuer(200, false).await;
        let store = Arc::new(RecordingStore::default());
        let tokens = Arc::new(ChatGptTokens::new(
            StoredTokens {
                access: "looks-fine".into(),
                refresh: "refresh-1".into(),
                expires_at: now_secs() + 3600,
                account_id: "acct_1".into(),
            },
            store.clone(),
            issuer,
        ));

        let p = ResponsesProvider::with_chatgpt(tokens)
            .unwrap()
            .with_base_url(server.url())
            .with_model("mock-model")
            .with_retry_delay_secs(0);

        let history = vec![user("hi")];
        let res = p
            .complete(request(&history), "msg-1", &NullSink::new())
            .await
            .unwrap();

        match &res.parts[0] {
            ContentPart::Text(t) => assert_eq!(t.text, "second time lucky"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(store.saved().len(), 1, "exactly one refresh");
    }

    /// A plan's window reopens in hours. Sitting through that would hold the
    /// turn open for the rest of the day, so a far-off reset is handed back
    /// with its duration rather than retried.
    #[test]
    fn a_far_off_rate_limit_reset_is_not_retryable_and_carries_its_wait() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "3600".parse().unwrap());

        let advised = advised_wait(&headers);
        assert_eq!(advised, Some(Duration::from_secs(3600)));

        let err = classify_status(429, "rate limited", advised);
        assert_eq!(err.retry_after(), Some(Duration::from_secs(3600)));
        assert!(
            !is_retryable(&err),
            "an hour-long reset must not be waited out"
        );
    }

    #[test]
    fn a_short_rate_limit_reset_is_still_retried() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-reset-requests", "12s".parse().unwrap());

        let err = classify_status(429, "slow down", advised_wait(&headers));

        assert_eq!(err.retry_after(), Some(Duration::from_secs(12)));
        assert!(is_retryable(&err));
    }

    /// No hint is not the same as a long wait: we fall back to our own backoff
    /// rather than giving up on the turn.
    #[test]
    fn a_rate_limit_without_a_hint_keeps_the_old_behaviour() {
        let err = classify_status(
            429,
            "slow down",
            advised_wait(&reqwest::header::HeaderMap::new()),
        );

        assert_eq!(err.retry_after(), None);
        assert!(is_retryable(&err));
    }

    #[test]
    fn every_turn_of_a_conversation_shares_one_cache_key() {
        let p = ResponsesProvider::with_api_key("k").unwrap();
        let first = vec![user("hi")];
        let later = vec![user("hi"), user("and again")];

        let a = p.build_body(&request(&first)).prompt_cache_key;
        let b = p.build_body(&request(&later)).prompt_cache_key;

        assert!(a.is_some());
        assert_eq!(a, b, "the key must not move as history grows");
        assert!(
            p.build_body(&request(&[])).prompt_cache_key.is_none(),
            "an empty history has no prefix worth caching"
        );
    }

    #[test]
    fn the_body_pins_the_three_fields_the_chatgpt_backend_requires() {
        let p = ResponsesProvider::with_api_key("k").unwrap();
        let history = vec![user("hi")];
        let body = p.build_body(&request(&history));

        assert!(!body.store, "store must be false: horsie owns the history");
        assert!(body.stream);
        assert_eq!(body.include, vec!["reasoning.encrypted_content"]);
    }

    #[test]
    fn a_pinned_tool_choice_uses_the_flat_shape() {
        let p = ResponsesProvider::with_api_key("k").unwrap();
        let history = vec![user("hi")];
        let mut req = request(&history);
        req.tools = vec![horsie_agentcore::ToolSpec {
            name: "echo".into(),
            description: "echoes".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        req.tool_choice = ToolChoice::Required("echo".into());

        let body = p.build_body(&req);

        assert_eq!(
            body.tool_choice,
            Some(serde_json::json!({"type": "function", "name": "echo"})),
            "the chat wire's nested {{function:{{name}}}} is rejected here"
        );
        assert_eq!(body.tools[0].kind, "function");
        assert_eq!(body.tools[0].name, "echo");
    }
}
