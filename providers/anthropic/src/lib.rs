use async_llm::{
    Client,
    types::{
        CacheControl, ContentBlockDelta, CreateMessagesRequestBuilder, MessageBuilder,
        MessageContent, MessageRole, MessagesStreamEvent, OutputConfig, Text, Thinking,
        ThinkingConfig, ToolResult, ToolUse,
    },
};
use async_trait::async_trait;
use horsie_agentcore::{
    AgentEvent, CompletionRequest, CompletionResponse, ContentBlockStopEvent, ContentPart,
    EventSink, LlmError, LlmProvider, Secret, StopReason, TextBlockStartEvent, TextChunkEvent,
    TextPart, ThinkingBlockStartEvent, ThinkingChunkEvent, ThinkingDialect, ThinkingEffort,
    ThinkingPart, ThinkingSignatureChunkEvent, ToolCallInputDeltaEvent, ToolCallPart,
    ToolCallStartEvent, ToolChoice, Usage,
};
use std::{collections::HashMap, env, time::Duration};
use tokio_stream::StreamExt;

pub const DEFAULT_MODEL: &str = "claude-3-5-sonnet-20241022";
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;
/// The `anthropic-version` header value sent on every request. Required by the API.
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_STREAM_RETRIES: u32 = 6;
const BACKOFF_BASE_SECS: u64 = 5;

pub fn env_base_url() -> Option<String> {
    env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Whether a classified error is worth another attempt.
///
/// Delegates to [`LlmError::is_transient`] so this layer and the agent actor's
/// retry loop cannot drift apart — two owners of "is this worth retrying" is how
/// a permanent 401 ends up retried seven times (#61 items 6 and 21).
fn is_retryable(error: &LlmError) -> bool {
    error.is_transient()
}

/// Map Anthropic's structured error `type` onto a classified [`LlmError`].
///
/// These identifiers come from Anthropic's published error schema, so matching
/// them is exact — unlike grepping a rendered message for status digits. Each
/// maps to the HTTP status Anthropic documents for it, so the status survives
/// into `LlmError::ApiError` instead of being discarded.
fn classify_error_type(error_type: &str, message: &str) -> LlmError {
    let status = match error_type {
        "overloaded_error" => return LlmError::Overloaded,
        "rate_limit_error" => return LlmError::RateLimit { retry_after: None },
        // Anthropic's own 500. Transient, so it classifies as overloaded rather
        // than as a permanent API error.
        "api_error" => return LlmError::Overloaded,
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        // An unrecognised type is still a real API response, not a network fault:
        // 502 says "the upstream answered with something we cannot classify".
        _ => 502,
    };
    LlmError::ApiError {
        status,
        message: format!("{error_type}: {message}"),
    }
}

/// Best-effort classification for variants that carry only a rendered string.
///
/// Looks for a known Anthropic error *type identifier* — a schema field name, not
/// a status digit — and falls back to an unclassified API error.
fn classify_opaque(message: &str) -> LlmError {
    const KNOWN: &[&str] = &[
        "overloaded_error",
        "rate_limit_error",
        "api_error",
        "invalid_request_error",
        "authentication_error",
        "permission_error",
        "not_found_error",
        "request_too_large",
    ];
    for token in KNOWN {
        if message.contains(token) {
            return classify_error_type(token, message);
        }
    }
    LlmError::ApiError {
        status: 502,
        message: message.to_string(),
    }
}

fn to_llm_error(e: async_llm::errors::AnthropicError) -> LlmError {
    use async_llm::errors::AnthropicError;
    match e {
        // Structured: the wire gave us the error type as a field, so no guessing.
        AnthropicError::StreamError(se) => classify_error_type(&se.error_type, &se.message),
        AnthropicError::NetworkError(re) => LlmError::Network(Box::new(re)),
        AnthropicError::Unauthorized => LlmError::ApiError {
            status: 401,
            message: "Unauthorized".into(),
        },
        // Upstream names this "malformed request" — it is the 400, and reporting it
        // as a *network* error told the user a permanent failure was transient.
        AnthropicError::BadRequest(m) => LlmError::ApiError {
            status: 400,
            message: m,
        },
        AnthropicError::ApiError(m) | AnthropicError::Unknown(m) => classify_opaque(&m),
        // Genuinely transport/parse level.
        AnthropicError::DeserializationError(de) => {
            LlmError::Network(Box::new(std::io::Error::other(de.to_string())))
        }
        AnthropicError::UnexpectedError => {
            LlmError::Network(Box::new(std::io::Error::other("unexpected error")))
        }
    }
}

/// Parse a streamed tool call's accumulated argument JSON.
///
/// An empty string means "no arguments" — backends send that for zero-parameter
/// tools — and becomes `{}`. Anything else that fails to parse is a malformed
/// response, and is reported as such rather than silently replaced.
fn parse_tool_input(raw: &str, tool: &str) -> Result<serde_json::Value, LlmError> {
    if raw.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::default()));
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

fn io_err(msg: impl std::fmt::Display) -> LlmError {
    LlmError::Network(Box::new(std::io::Error::other(msg.to_string())))
}

/// Bounds TCP + TLS setup. A peer that never completes a handshake is dead, not slow.
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// Bounds *idle* time between reads, not the total call.
///
/// A total `.timeout()` would kill legitimately long generations; this resets on
/// every chunk, so a slow-but-alive stream runs indefinitely while a stalled one
/// is bounded (#61 item 5).
const DEFAULT_READ_TIMEOUT_SECS: u64 = 120;

pub struct AnthropicProvider {
    client: Client,
    model: String,
    api_key: Option<Secret>,
    base_url: Option<String>,
    session_id: Option<String>,
    thinking_budget: Option<u32>,
    /// Wire encoding for this model's thinking control.
    thinking_dialect: ThinkingDialect,
    /// Retain provider thinking-block signatures captured from this endpoint.
    keep_thinking_signature: bool,
    max_tokens: Option<u32>,
    retry_base_secs: u64,
    read_timeout_secs: u64,
}

impl AnthropicProvider {
    fn build_client(
        api_key: Option<&str>,
        base_url: Option<&str>,
        session_id: Option<&str>,
        read_timeout_secs: u64,
    ) -> Result<Client, LlmError> {
        // Every other HTTP client in the repo sets a timeout — github/api.rs
        // (15s), velos/client.rs (10s), mcp/oauth.rs (15s) — so the absence here
        // was an oversight rather than a decision (#61 item 5).
        let mut http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .read_timeout(Duration::from_secs(read_timeout_secs));
        if let Some(sid) = session_id {
            let mut headers = reqwest::header::HeaderMap::new();
            if let Ok(val) = reqwest::header::HeaderValue::from_str(sid) {
                headers.insert("X-Session-Id", val);
            }
            http = http.default_headers(headers);
        }
        let http_client = http.build().map_err(|e| LlmError::Network(Box::new(e)))?;

        let mut builder = Client::builder();
        builder.http_client(http_client);
        // The async-anthropic `Client` derives its builder with `version:
        // #[builder(default)]`, which yields an empty string — NOT the
        // `"2023-06-01"` from its `Default` impl. An empty `anthropic-version`
        // header makes the streaming endpoint reject every request with
        // `400 "anthropic-version: header is required"`, so set it explicitly.
        builder.version(ANTHROPIC_VERSION);
        if let Some(url) = base_url {
            builder.base_url(url);
        }
        if let Some(key) = api_key {
            builder.api_key(key);
        }
        builder.build().map_err(io_err)
    }

    pub fn new() -> Result<Self, LlmError> {
        let base_url = env_base_url();
        let client =
            Self::build_client(None, base_url.as_deref(), None, DEFAULT_READ_TIMEOUT_SECS)?;
        Ok(Self {
            client,
            model: DEFAULT_MODEL.into(),
            api_key: None,
            base_url,
            session_id: None,
            thinking_budget: None,
            thinking_dialect: ThinkingDialect::NoControl,
            keep_thinking_signature: false,
            max_tokens: None,
            retry_base_secs: BACKOFF_BASE_SECS,
            read_timeout_secs: DEFAULT_READ_TIMEOUT_SECS,
        })
    }

    pub fn with_api_key(key: impl Into<Secret>) -> Result<Self, LlmError> {
        let key = key.into();
        let base_url = env_base_url();
        let client = Self::build_client(
            Some(key.expose()),
            base_url.as_deref(),
            None,
            DEFAULT_READ_TIMEOUT_SECS,
        )?;
        Ok(Self {
            client,
            model: DEFAULT_MODEL.into(),
            api_key: Some(key),
            base_url,
            session_id: None,
            thinking_budget: None,
            thinking_dialect: ThinkingDialect::NoControl,
            keep_thinking_signature: false,
            max_tokens: None,
            retry_base_secs: BACKOFF_BASE_SECS,
            read_timeout_secs: DEFAULT_READ_TIMEOUT_SECS,
        })
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self.rebuild_client();
        self
    }

    #[must_use]
    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self.rebuild_client();
        self
    }

    #[must_use]
    pub fn with_max_tokens(mut self, n: Option<u32>) -> Self {
        self.max_tokens = n;
        self
    }

    #[must_use]
    pub fn with_thinking(mut self, budget_tokens: u32) -> Self {
        self.thinking_budget = Some(budget_tokens);
        self
    }

    /// Set the wire encoding used for this model's thinking control.
    #[must_use]
    pub fn with_thinking_dialect(mut self, dialect: ThinkingDialect) -> Self {
        self.thinking_dialect = dialect;
        self
    }

    /// Retain provider thinking-block signatures on captured thinking parts.
    ///
    /// Genuine Anthropic validates these on replay, so real Anthropic providers
    /// must enable this. Anthropic-compatible endpoints do not: verified
    /// 2026-07-27 against `https://api.kimi.com/coding/` (model `k3`), where
    /// omitted, empty, altered, and wholly removed signatures were all accepted
    /// with 200 — including inside tool-use loops. Default off, because the
    /// blobs run 4-13 KB each and no client reads them.
    #[must_use]
    pub fn with_keep_thinking_signature(mut self, keep: bool) -> Self {
        self.keep_thinking_signature = keep;
        self
    }

    #[must_use]
    pub fn with_retry_delay_secs(mut self, secs: u64) -> Self {
        self.retry_base_secs = secs;
        self
    }

    /// Bound how long the client waits on a silent peer.
    ///
    /// Idle time between reads, not total call duration — a long generation that
    /// keeps streaming is unaffected. Configurable because "how long may a model
    /// think before its first token" is a per-deployment answer, and because
    /// tests need a deadline they can actually reach.
    #[must_use]
    pub fn with_read_timeout_secs(mut self, secs: u64) -> Self {
        self.read_timeout_secs = secs;
        self.rebuild_client();
        self
    }

    fn rebuild_client(&mut self) {
        match Self::build_client(
            self.api_key.as_ref().map(Secret::expose),
            self.base_url.as_deref(),
            self.session_id.as_deref(),
            self.read_timeout_secs,
        ) {
            Ok(c) => self.client = c,
            Err(e) => tracing::warn!("failed to rebuild Anthropic client: {e}"),
        }
    }

    fn to_api_role(role: &horsie_models::agent::Role) -> MessageRole {
        match role {
            horsie_models::agent::Role::Assistant => MessageRole::Assistant,
            horsie_models::agent::Role::User | horsie_models::agent::Role::Tool => {
                MessageRole::User
            }
        }
    }

    /// Translate a canonical effort into this model's wire fields. Returns the
    /// `thinking` config and the `output_config`, either of which may be absent.
    fn encode_thinking(
        dialect: ThinkingDialect,
        effort: Option<ThinkingEffort>,
        budget_tokens: Option<u32>,
    ) -> (Option<ThinkingConfig>, Option<OutputConfig>) {
        let Some(effort) = effort else {
            return (None, None);
        };
        let as_effort = || {
            Some(OutputConfig {
                effort: Some(effort.as_str().to_string()),
            })
        };
        match dialect {
            ThinkingDialect::AnthropicEffort => {
                if effort.is_none_effort() {
                    (Some(ThinkingConfig::Disabled), None)
                } else {
                    (None, as_effort())
                }
            }
            // Fable 5 rejects an explicit disable; `supports()` blocks `none` at
            // config time, so only effort values reach here.
            ThinkingDialect::AnthropicAlwaysOn => (None, as_effort()),
            ThinkingDialect::AnthropicBudget => {
                if effort.is_none_effort() {
                    (Some(ThinkingConfig::Disabled), None)
                } else {
                    (
                        budget_tokens
                            .map(|budget_tokens| ThinkingConfig::Enabled { budget_tokens }),
                        None,
                    )
                }
            }
            ThinkingDialect::ZaiThinking | ThinkingDialect::KimiThinking => {
                if effort.is_none_effort() {
                    (Some(ThinkingConfig::Disabled), None)
                } else {
                    (None, None)
                }
            }
            ThinkingDialect::OpenAiEffort | ThinkingDialect::NoControl => (None, None),
        }
    }

    /// Build the thinking part for one assembled block, honoring the
    /// signature-retention policy. `None` when the block carried no text.
    fn thinking_part(text: &str, signature: &str, keep_signature: bool) -> Option<ContentPart> {
        if text.is_empty() {
            return None;
        }
        Some(ContentPart::Thinking(ThinkingPart {
            text: text.to_string(),
            signature: if keep_signature && !signature.is_empty() {
                Some(signature.to_string())
            } else {
                None
            },
        }))
    }

    fn parts_to_api_content(parts: &[ContentPart]) -> async_llm::types::MessageContentList {
        use async_llm::types::MessageContentList;
        let items: Vec<MessageContent> = parts
            .iter()
            .map(|p| match p {
                ContentPart::Text(t) => MessageContent::Text(Text {
                    text: t.text.clone(),
                    ..Default::default()
                }),
                ContentPart::ToolCall(tc) => MessageContent::ToolUse(ToolUse {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.input.clone(),
                    ..Default::default()
                }),
                ContentPart::ToolResult(tr) => MessageContent::ToolResult(ToolResult {
                    tool_use_id: tr.tool_call_id.clone(),
                    content: Some(tr.output.clone()),
                    is_error: tr.is_error,
                    ..Default::default()
                }),
                ContentPart::Thinking(th) => MessageContent::Thinking(Thinking {
                    thinking: th.text.clone(),
                    signature: th.signature.clone(),
                    ..Default::default()
                }),
                // Flattened to the text block it has always been: this part is
                // provenance for clients, not a new thing to show the model.
                ContentPart::SubAgentResult(r) => MessageContent::Text(Text {
                    text: r.to_wire_text(),
                    ..Default::default()
                }),
            })
            .collect();
        MessageContentList(items)
    }

    fn mark_last_message_cacheable(messages: &mut [async_llm::types::Message]) {
        let Some(last) = messages.last_mut() else {
            return;
        };
        let Some(block) = last.content.last_mut() else {
            return;
        };
        let cc = Some(CacheControl::ephemeral());
        match block {
            MessageContent::Text(t) => t.cache_control = cc,
            MessageContent::ToolUse(tu) => tu.cache_control = cc,
            MessageContent::ToolResult(tr) => tr.cache_control = cc,
            MessageContent::Thinking(th) => th.cache_control = cc,
        }
    }

    fn mark_last_tool_cacheable(tools: &mut [serde_json::Map<String, serde_json::Value>]) {
        if let Some(last) = tools.last_mut() {
            last.insert(
                "cache_control".into(),
                serde_json::json!({"type": "ephemeral"}),
            );
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        // 1. Convert messages
        let mut api_messages: Vec<async_llm::types::Message> = request
            .messages
            .iter()
            .map(|m| {
                MessageBuilder::default()
                    .role(Self::to_api_role(&m.role))
                    .content(Self::parts_to_api_content(&m.parts))
                    .build()
                    .map_err(io_err)
            })
            .collect::<Result<Vec<_>, _>>()?;

        Self::mark_last_message_cacheable(&mut api_messages);

        // 2. Convert tools
        let mut tool_defs: Vec<serde_json::Map<String, serde_json::Value>> = request
            .tools
            .iter()
            .map(|t| {
                let mut m = serde_json::Map::new();
                m.insert("name".into(), serde_json::json!(t.name));
                m.insert("description".into(), serde_json::json!(t.description));
                m.insert("input_schema".into(), t.input_schema.clone());
                m
            })
            .collect();
        Self::mark_last_tool_cacheable(&mut tool_defs);

        // 3. Build request
        let max_tokens = self
            .max_tokens
            .or(request.max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS) as i32;

        let mut builder = CreateMessagesRequestBuilder::default();
        builder
            .model(&self.model)
            .messages(api_messages)
            .max_tokens(max_tokens);
        if let Some(sys) = &request.system {
            builder.system(sys.clone());
        }
        if !tool_defs.is_empty() {
            builder.tools(tool_defs);
            match &request.tool_choice {
                ToolChoice::Auto => {}
                ToolChoice::Any => {
                    builder.tool_choice(async_llm::types::ToolChoice::Any);
                }
                ToolChoice::Required(name) => {
                    builder.tool_choice(async_llm::types::ToolChoice::Tool(name.clone()));
                }
            }
        }
        let (thinking_cfg, output_config) = Self::encode_thinking(
            self.thinking_dialect,
            request.thinking_effort,
            self.thinking_budget,
        );
        if let Some(t) = thinking_cfg {
            builder.thinking(t);
        }
        if let Some(oc) = output_config {
            builder.output_config(oc);
        }
        let api_request = builder.build().map_err(io_err)?;

        // 4. Stream with retry (only when no content has been emitted yet)
        let mut text_blocks: HashMap<usize, String> = HashMap::new();
        let mut tool_blocks: HashMap<usize, (String, String, String)> = HashMap::new();
        let mut thinking_blocks: HashMap<usize, (String, String)> = HashMap::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut cache_creation_tokens: Option<u32> = None;
        let mut cache_read_tokens: Option<u32> = None;
        let mut last_error: Option<LlmError> = None;
        // Did the backend actually end the turn? A stream that just stops — the
        // connection dropped mid-response — must not be mistaken for a completed
        // turn (#61 item 1). `MessageDelta` carries the stop_reason and
        // `MessageStop` closes the message; either is a genuine terminal event.
        let mut saw_terminal = false;

        'retry: for attempt in 0..=MAX_STREAM_RETRIES {
            if attempt > 0 {
                let delay = self.retry_base_secs * 2u64.pow(attempt - 1);
                tracing::warn!(
                    attempt,
                    delay_secs = delay,
                    "Anthropic overload/rate-limit, retrying"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
                text_blocks.clear();
                tool_blocks.clear();
                thinking_blocks.clear();
                stop_reason = StopReason::EndTurn;
                input_tokens = 0;
                output_tokens = 0;
                cache_creation_tokens = None;
                cache_read_tokens = None;
            }

            let mut stream = self
                .client
                .messages()
                .create_stream(api_request.clone())
                .await;

            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(e) => e,
                    Err(e) => {
                        // Classify once, then decide retryability from the result.
                        let classified = to_llm_error(e);
                        if is_retryable(&classified)
                            && text_blocks.is_empty()
                            && tool_blocks.is_empty()
                            && thinking_blocks.is_empty()
                        {
                            last_error = Some(classified);
                            continue 'retry;
                        }
                        return Err(classified);
                    }
                };

                // Map each raw stream event to at most one AgentEvent while folding
                // it into the per-block accumulators used to rebuild the final
                // Message below. Emission happens once, after the match, so the
                // backpressure `?` lives in a single chokepoint.
                let mid = message_id.to_string();
                let to_emit: Option<AgentEvent> = match event {
                    MessagesStreamEvent::MessageStart { message, usage: _ } => {
                        if let Some(u) = &message.usage {
                            input_tokens = u.input_tokens.unwrap_or(0);
                            cache_creation_tokens = u.cache_creation_input_tokens;
                            cache_read_tokens = u.cache_read_input_tokens;
                        }
                        None
                    }
                    MessagesStreamEvent::ContentBlockStart {
                        index,
                        content_block,
                    } => match content_block {
                        MessageContent::Text(_) => {
                            text_blocks.insert(index, String::new());
                            Some(AgentEvent::TextBlockStart(TextBlockStartEvent {
                                message_id: mid,
                                index: index as u32,
                            }))
                        }
                        MessageContent::ToolUse(tu) => {
                            let ev = AgentEvent::ToolCallStart(ToolCallStartEvent {
                                message_id: mid,
                                index: index as u32,
                                tool_call_id: tu.id.clone(),
                                name: tu.name.clone(),
                            });
                            tool_blocks.insert(index, (tu.id, tu.name, String::new()));
                            Some(ev)
                        }
                        MessageContent::Thinking(_) => {
                            thinking_blocks.insert(index, (String::new(), String::new()));
                            Some(AgentEvent::ThinkingBlockStart(ThinkingBlockStartEvent {
                                message_id: mid,
                                index: index as u32,
                            }))
                        }
                        MessageContent::ToolResult(_) => None,
                    },
                    MessagesStreamEvent::ContentBlockDelta { index, delta } => match delta {
                        ContentBlockDelta::TextDelta { text } => {
                            if let Some(acc) = text_blocks.get_mut(&index) {
                                acc.push_str(&text);
                            }
                            Some(AgentEvent::TextChunk(TextChunkEvent {
                                message_id: mid,
                                index: index as u32,
                                text,
                            }))
                        }
                        ContentBlockDelta::InputJsonDelta { partial_json } => {
                            match tool_blocks.get_mut(&index) {
                                Some((id, _, acc)) => {
                                    acc.push_str(&partial_json);
                                    Some(AgentEvent::ToolCallInputDelta(ToolCallInputDeltaEvent {
                                        message_id: mid,
                                        index: index as u32,
                                        tool_call_id: id.clone(),
                                        delta: partial_json,
                                    }))
                                }
                                None => None,
                            }
                        }
                        ContentBlockDelta::ThinkingDelta { thinking } => {
                            if let Some((acc, _)) = thinking_blocks.get_mut(&index) {
                                acc.push_str(&thinking);
                            }
                            Some(AgentEvent::ThinkingChunk(ThinkingChunkEvent {
                                message_id: mid,
                                index: index as u32,
                                text: thinking,
                            }))
                        }
                        ContentBlockDelta::SignatureDelta { signature } => {
                            if let Some((_, acc_sig)) = thinking_blocks.get_mut(&index) {
                                acc_sig.push_str(&signature);
                            }
                            Some(AgentEvent::ThinkingSignatureChunk(
                                ThinkingSignatureChunkEvent {
                                    message_id: mid,
                                    index: index as u32,
                                    signature,
                                },
                            ))
                        }
                    },
                    MessagesStreamEvent::ContentBlockStop { index } => {
                        Some(AgentEvent::ContentBlockStop(ContentBlockStopEvent {
                            message_id: mid,
                            index: index as u32,
                        }))
                    }
                    MessagesStreamEvent::MessageDelta { delta, usage } => {
                        saw_terminal = true;
                        stop_reason = match delta.stop_reason.as_deref() {
                            Some("tool_use") => StopReason::ToolUse,
                            Some("max_tokens") => StopReason::MaxTokens,
                            Some(_) | None => StopReason::EndTurn,
                        };
                        if let Some(u) = usage {
                            output_tokens = u.output_tokens.unwrap_or(output_tokens);
                            // `message_delta` carries the *final* accounting and
                            // supersedes `message_start` for every field it sets.
                            // Anthropic itself reports the cache split up front, but
                            // Anthropic-compatible endpoints need not: verified
                            // 2026-07-28 against `https://api.kimi.com/coding/`
                            // (model `k3`), where `message_start` always reports
                            // `input_tokens` uncached with both cache counters at 0,
                            // and only `message_delta` carries the real split (e.g.
                            // start `input=7507, read=0` vs delta `input=83,
                            // read=7424` for the same call). Reading the cache fields
                            // from `message_start` alone reports every kimi turn as a
                            // 100% cache miss and bills the whole prefix at the fresh
                            // input rate. Each field falls back to the `message_start`
                            // value when the delta omits it, so providers that only
                            // send `output_tokens` here are unaffected.
                            input_tokens = u.input_tokens.unwrap_or(input_tokens);
                            cache_creation_tokens =
                                u.cache_creation_input_tokens.or(cache_creation_tokens);
                            cache_read_tokens = u.cache_read_input_tokens.or(cache_read_tokens);
                        }
                        None
                    }
                    MessagesStreamEvent::MessageStop => {
                        saw_terminal = true;
                        None
                    }
                };

                if let Some(ev) = to_emit {
                    events.emit(ev).await?;
                }
            }

            break 'retry;
        }

        if text_blocks.is_empty()
            && tool_blocks.is_empty()
            && thinking_blocks.is_empty()
            && let Some(e) = last_error.take()
        {
            return Err(e);
        }

        // A stream that ended without its terminal event is a dropped connection,
        // not an answer. Returning what arrived would journal a truncated (or
        // empty) assistant turn and present it to the user as success (#61 item 1).
        if !saw_terminal {
            return Err(last_error.take().unwrap_or_else(|| {
                io_err("stream ended without a terminal event (connection dropped mid-response)")
            }));
        }

        // 5. Assemble parts in block-index order
        let mut all_indices: Vec<usize> = text_blocks
            .keys()
            .chain(tool_blocks.keys())
            .chain(thinking_blocks.keys())
            .copied()
            .collect();
        all_indices.sort_unstable();
        all_indices.dedup();

        let mut parts = Vec::new();
        for idx in all_indices {
            if let Some(text) = text_blocks.get(&idx) {
                if !text.is_empty() {
                    parts.push(ContentPart::Text(TextPart { text: text.clone() }));
                }
            } else if let Some((id, name, json_str)) = tool_blocks.get(&idx) {
                // An unparseable input is never substituted with an empty object:
                // dispatching a tool with fabricated arguments turns a provider
                // failure into a confusing tool failure, and can run the tool with
                // arguments the model never chose (#61 item 1).
                let input = match parse_tool_input(json_str, name) {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
                parts.push(ContentPart::ToolCall(ToolCallPart {
                    id: id.clone(),
                    name: name.clone(),
                    input,
                }));
            } else if let Some((thinking, signature)) = thinking_blocks.get(&idx)
                && let Some(part) =
                    Self::thinking_part(thinking, signature, self.keep_thinking_signature)
            {
                parts.push(part);
            }
        }

        Ok(CompletionResponse {
            parts,
            stop_reason,
            usage: Usage {
                // Anthropic's `input_tokens` excludes cache-read/cache-creation
                // tokens (they ride in separate fields on the wire) — add them
                // back in so `Usage.input_tokens` means "full prompt size" the
                // same way it already does on the OpenAI wire.
                input_tokens: input_tokens
                    + cache_creation_tokens.unwrap_or(0)
                    + cache_read_tokens.unwrap_or(0),
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
            },
        })
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

    use async_llm::errors::{AnthropicError, StreamError};

    #[test]
    fn bad_request_keeps_its_status_instead_of_becoming_a_network_error() {
        // #61 item 6: this used to be `LlmError::Network`, so a permanent 400 —
        // context-length exceeded, a malformed tool schema, an unanswered
        // tool_use — was reported to the user as a transient network failure.
        let e = to_llm_error(AnthropicError::BadRequest("context too long".into()));
        assert!(matches!(e, LlmError::ApiError { status: 400, .. }), "{e:?}");
        assert!(!is_retryable(&e), "a 400 must not be retried");
    }

    #[test]
    fn a_permission_error_classifies_as_403() {
        let e = classify_error_type("permission_error", "no access to this model");
        assert!(matches!(e, LlmError::ApiError { status: 403, .. }), "{e:?}");
        assert!(!is_retryable(&e));
    }

    #[test]
    fn structured_stream_errors_classify_from_their_type_field() {
        let overloaded = to_llm_error(AnthropicError::StreamError(StreamError {
            error_type: "overloaded_error".into(),
            message: "try later".into(),
        }));
        assert!(matches!(overloaded, LlmError::Overloaded));
        assert!(is_retryable(&overloaded));

        let rate = to_llm_error(AnthropicError::StreamError(StreamError {
            error_type: "rate_limit_error".into(),
            message: "slow down".into(),
        }));
        assert!(matches!(rate, LlmError::RateLimit { .. }));
        assert!(is_retryable(&rate));

        let bad = to_llm_error(AnthropicError::StreamError(StreamError {
            error_type: "invalid_request_error".into(),
            message: "context too long".into(),
        }));
        assert!(
            matches!(bad, LlmError::ApiError { status: 400, .. }),
            "{bad:?}"
        );
        assert!(!is_retryable(&bad));
    }

    #[test]
    fn digits_in_an_error_message_no_longer_fake_a_rate_limit() {
        // The old classifier grepped the rendered message for "429" / "529", so a
        // request id, model name or token count containing those digits was
        // misread as a retryable rate limit. Exactly the string-matching failure
        // #61 item 6 describes.
        let e = to_llm_error(AnthropicError::StreamError(StreamError {
            error_type: "invalid_request_error".into(),
            message: "request req_4291 exceeded 529 tokens".into(),
        }));
        assert!(
            matches!(e, LlmError::ApiError { status: 400, .. }),
            "digits in the body must not classify as rate limit / overloaded: {e:?}"
        );
        assert!(!is_retryable(&e));
    }

    #[test]
    fn an_unknown_error_type_is_an_api_error_not_a_network_error() {
        let e = classify_error_type("some_new_error", "who knows");
        assert!(matches!(e, LlmError::ApiError { status: 502, .. }), "{e:?}");
    }

    #[test]
    fn opaque_errors_still_recover_a_known_type_token() {
        let e = classify_opaque(r#"{"type":"overloaded_error","message":"busy"}"#);
        assert!(matches!(e, LlmError::Overloaded), "{e:?}");
    }
    use super::*;

    #[test]
    fn test_to_api_role_user() {
        assert!(matches!(
            AnthropicProvider::to_api_role(&horsie_models::agent::Role::User),
            MessageRole::User
        ));
    }

    #[test]
    fn test_to_api_role_assistant() {
        assert!(matches!(
            AnthropicProvider::to_api_role(&horsie_models::agent::Role::Assistant),
            MessageRole::Assistant
        ));
    }

    #[test]
    fn test_to_api_role_tool_maps_to_user() {
        assert!(matches!(
            AnthropicProvider::to_api_role(&horsie_models::agent::Role::Tool),
            MessageRole::User
        ));
    }

    #[test]
    fn test_parts_to_api_content_text() {
        let parts = vec![ContentPart::Text(TextPart {
            text: "hello".into(),
        })];
        let list = AnthropicProvider::parts_to_api_content(&parts);
        assert_eq!(list.len(), 1);
        assert!(matches!(&list[0], MessageContent::Text(t) if t.text == "hello"));
    }

    /// The whole point of the structured part is that the model never learns
    /// about it. Pinned against the literal string, not just `to_wire_text`,
    /// so a change to the format has to be a deliberate edit here.
    #[test]
    fn test_parts_to_api_content_subagent_result_is_a_text_block() {
        let parts = vec![ContentPart::SubAgentResult(
            horsie_models::agent::SubAgentResultPart {
                subagent_id: "id".into(),
                label: "audit".into(),
                status: "completed".into(),
                text: "three stale crates".into(),
                spawned_at_ms: 100,
                ended_at_ms: 400,
            },
        )];
        let list = AnthropicProvider::parts_to_api_content(&parts);
        assert_eq!(list.len(), 1);
        assert!(matches!(&list[0], MessageContent::Text(t)
                if t.text == "[subagent \"audit\" completed]\n\nthree stale crates"));
    }

    #[test]
    fn test_parts_to_api_content_tool_result() {
        let parts = vec![ContentPart::ToolResult(
            horsie_models::agent::ToolResultPart {
                tool_call_id: "tc1".into(),
                output: "result".into(),
                is_error: false,
            },
        )];
        let list = AnthropicProvider::parts_to_api_content(&parts);
        assert_eq!(list.len(), 1);
        assert!(matches!(&list[0], MessageContent::ToolResult(tr) if tr.tool_use_id == "tc1"));
    }

    #[test]
    fn test_parts_to_api_content_thinking_echoes_signature() {
        let parts = vec![ContentPart::Thinking(ThinkingPart {
            text: "think".into(),
            signature: Some("sig123".into()),
        })];
        let list = AnthropicProvider::parts_to_api_content(&parts);
        assert_eq!(list.len(), 1);
        assert!(
            matches!(&list[0], MessageContent::Thinking(t) if t.thinking == "think" && t.signature.as_deref() == Some("sig123"))
        );
    }

    #[test]
    fn test_empty_env_base_url_treated_as_unset() {
        let original = env::var("ANTHROPIC_BASE_URL").ok();
        unsafe {
            env::set_var("ANTHROPIC_BASE_URL", "");
        }
        assert_eq!(env_base_url(), None);
        unsafe {
            env::set_var("ANTHROPIC_BASE_URL", "https://example.com");
        }
        assert_eq!(env_base_url(), Some("https://example.com".into()));
        unsafe {
            env::remove_var("ANTHROPIC_BASE_URL");
        }
        assert_eq!(env_base_url(), None);
        if let Some(v) = original {
            unsafe {
                env::set_var("ANTHROPIC_BASE_URL", v);
            }
        }
    }

    #[test]
    fn thinking_part_keeps_signature_when_enabled() {
        let part = AnthropicProvider::thinking_part("reasoning", "sig-blob", true)
            .expect("non-empty thinking yields a part");
        match part {
            ContentPart::Thinking(th) => {
                assert_eq!(th.text, "reasoning");
                assert_eq!(th.signature.as_deref(), Some("sig-blob"));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn thinking_part_drops_signature_when_disabled() {
        let part = AnthropicProvider::thinking_part("reasoning", "sig-blob", false)
            .expect("non-empty thinking yields a part");
        match part {
            ContentPart::Thinking(th) => {
                assert_eq!(th.text, "reasoning");
                assert_eq!(th.signature, None);
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn thinking_part_normalizes_empty_signature_to_none() {
        let part = AnthropicProvider::thinking_part("reasoning", "", true)
            .expect("non-empty thinking yields a part");
        match part {
            ContentPart::Thinking(th) => assert_eq!(th.signature, None),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn thinking_part_skips_empty_thinking() {
        assert!(AnthropicProvider::thinking_part("", "sig-blob", true).is_none());
    }

    #[test]
    fn keep_thinking_signature_defaults_off() {
        let p = AnthropicProvider::new().expect("provider builds without a key");
        assert!(!p.keep_thinking_signature);
    }

    #[test]
    fn with_keep_thinking_signature_enables_retention() {
        let p = AnthropicProvider::new()
            .expect("provider builds without a key")
            .with_keep_thinking_signature(true);
        assert!(p.keep_thinking_signature);
    }

    #[test]
    fn parts_to_api_content_omits_absent_thinking_signature() {
        let parts = vec![ContentPart::Thinking(ThinkingPart {
            text: "reasoning".into(),
            signature: None,
        })];
        let list = AnthropicProvider::parts_to_api_content(&parts);
        let json = serde_json::to_value(&list[0]).expect("serializes");
        assert!(
            json.get("signature").is_none(),
            "an absent signature must not be sent as an empty string: {json}"
        );
    }

    fn effort(v: &str) -> ThinkingEffort {
        ThinkingEffort::parse(v).expect("canonical effort")
    }

    #[test]
    fn anthropic_effort_dialect_sets_output_config() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::AnthropicEffort,
            Some(effort("high")),
            None,
        );
        assert_eq!(output.expect("set").effort.as_deref(), Some("high"));
        assert!(thinking.is_none());
    }

    #[test]
    fn anthropic_effort_dialect_maps_none_to_disabled() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::AnthropicEffort,
            Some(effort("none")),
            None,
        );
        assert!(matches!(thinking, Some(ThinkingConfig::Disabled)));
        assert!(output.is_none());
    }

    #[test]
    fn always_on_dialect_never_disables() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::AnthropicAlwaysOn,
            Some(effort("max")),
            None,
        );
        assert!(thinking.is_none(), "thinking must be omitted entirely");
        assert_eq!(output.expect("set").effort.as_deref(), Some("max"));
    }

    #[test]
    fn budget_dialect_uses_budget_tokens() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::AnthropicBudget,
            Some(effort("high")),
            Some(4096),
        );
        assert!(matches!(
            thinking,
            Some(ThinkingConfig::Enabled {
                budget_tokens: 4096
            })
        ));
        assert!(
            output.is_none(),
            "budget-era models reject output_config.effort"
        );
    }

    #[test]
    fn no_control_dialect_sends_nothing() {
        let (thinking, output) = AnthropicProvider::encode_thinking(
            ThinkingDialect::NoControl,
            Some(effort("high")),
            None,
        );
        assert!(thinking.is_none());
        assert!(output.is_none());
    }

    #[test]
    fn absent_effort_sends_nothing() {
        let (thinking, output) =
            AnthropicProvider::encode_thinking(ThinkingDialect::AnthropicEffort, None, None);
        assert!(thinking.is_none());
        assert!(output.is_none());
    }
}
