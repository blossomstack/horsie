//! An `LlmProvider` for OpenAI-compatible `/v1/chat/completions` backends.
//!
//! One implementation targets OpenAI, Ollama, vLLM, llama.cpp, OpenRouter and
//! DeepSeek. It never emits `cache_control`, never replays thinking blocks, and
//! maps tool results onto their own `role: "tool"` messages — see [`wire`].
//!
//! Parsing is deliberately forgiving: a stream frame that fails to deserialize
//! is skipped rather than fatal, because compatible backends emit keepalives and
//! vendor-specific frames that are not chat chunks.

pub mod wire;

use async_trait::async_trait;
use futures_util::StreamExt;
use horsie_agentcore::{
    AgentEvent, CompletionRequest, CompletionResponse, ContentBlockStopEvent, ContentPart,
    EventSink, LlmError, LlmProvider, ProviderCapabilities, Secret, StopReason,
    TextBlockStartEvent, TextChunkEvent, TextPart, ThinkingBlockStartEvent, ThinkingChunkEvent,
    ThinkingPart, ToolCallInputDeltaEvent, ToolCallPart, ToolCallStartEvent, ToolChoice, Usage,
};
use reqwest_eventsource::{Event, EventSource};
use std::{collections::BTreeMap, env, time::Duration};
use wire::{ChatChunk, ChatMessage, ChatRequest, FunctionDef, ToolDef, to_wire_messages};

pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const MAX_STREAM_RETRIES: u32 = 6;
const BACKOFF_BASE_SECS: u64 = 5;

#[must_use]
pub fn env_base_url() -> Option<String> {
    env::var("OPENAI_BASE_URL").ok().filter(|s| !s.is_empty())
}

fn io_err(msg: impl std::fmt::Display) -> LlmError {
    LlmError::Network(Box::new(std::io::Error::other(msg.to_string())))
}

/// Map an HTTP status onto a classified error.
///
/// Unlike the Anthropic provider, which greps error *bodies* for
/// `overloaded_error`/`rate_limit_error`, OpenAI-compatible backends signal with
/// real status codes — so classification is exact rather than string-matched.
/// Getting this wrong is not cosmetic: a 429 misfiled as `Network` is not
/// retried, and silently consumes the caller's budget.
fn classify_status(status: u16, body: &str) -> LlmError {
    match status {
        429 => LlmError::RateLimit { retry_after: None },
        500 | 502 | 503 | 504 | 529 => LlmError::Overloaded,
        _ => LlmError::ApiError {
            status,
            message: body.to_string(),
        },
    }
}

fn is_retryable(e: &LlmError) -> bool {
    matches!(e, LlmError::RateLimit { .. } | LlmError::Overloaded)
}

pub struct OpenAiProvider {
    http: reqwest::Client,
    model: String,
    api_key: Option<Secret>,
    base_url: String,
    max_tokens: Option<u32>,
    retry_base_secs: u64,
}

impl OpenAiProvider {
    fn build(api_key: Option<Secret>) -> Result<Self, LlmError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .build()
                .map_err(|e| LlmError::Network(Box::new(e)))?,
            model: DEFAULT_MODEL.to_string(),
            api_key,
            base_url: env_base_url().unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            max_tokens: None,
            retry_base_secs: BACKOFF_BASE_SECS,
        })
    }

    /// Reads `OPENAI_API_KEY` if set. An absent key is not an error — a local
    /// Ollama or llama.cpp server needs none.
    pub fn new() -> Result<Self, LlmError> {
        let key = env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Secret::from);
        Self::build(key)
    }

    pub fn with_api_key(key: impl Into<Secret>) -> Result<Self, LlmError> {
        Self::build(Some(key.into()))
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
    pub fn with_retry_delay_secs(mut self, secs: u64) -> Self {
        self.retry_base_secs = secs;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> ChatRequest {
        let mut messages = Vec::new();
        // OpenAI carries the system prompt as the first message, not a
        // top-level field the way Anthropic does.
        if let Some(sys) = &request.system {
            messages.push(ChatMessage::new("system", Some(sys.clone())));
        }
        messages.extend(to_wire_messages(request.messages));

        let tools: Vec<ToolDef> = request
            .tools
            .iter()
            .map(|t| ToolDef {
                kind: "function".to_string(),
                function: FunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect();

        let tool_choice = if tools.is_empty() {
            None
        } else {
            match &request.tool_choice {
                ToolChoice::Auto => None,
                ToolChoice::Any => Some(serde_json::json!("required")),
                ToolChoice::Required(name) => Some(serde_json::json!({
                    "type": "function",
                    "function": { "name": name }
                })),
            }
        };

        ChatRequest {
            model: self.model.clone(),
            messages,
            stream: true,
            max_tokens: self
                .max_tokens
                .or(request.max_tokens)
                .or(Some(DEFAULT_MAX_TOKENS)),
            tools,
            tool_choice,
        }
    }
}

/// Accumulator for one streamed tool call. Arguments arrive fragmented and are
/// only parseable once the stream ends.
#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
    started: bool,
}

/// Everything folded out of one attempt's stream.
struct StreamState {
    /// Reasoning trace, when the backend streams one (DeepSeek/vLLM/OpenRouter).
    /// Surfaced as a `ThinkingPart` for display, but never sent back on the next
    /// turn — see [`wire::to_wire_messages`], which drops thinking parts.
    reasoning: String,
    text: String,
    tools: BTreeMap<usize, ToolAcc>,
    usage: Usage,
    reasoning_started: bool,
    text_started: bool,
    /// The content-block index the text uses. Reasoning always precedes text on
    /// these backends, so when a thinking block opens first the text becomes
    /// block 1; otherwise block 0.
    text_index: u32,
    emitted_anything: bool,
}

impl Default for StreamState {
    fn default() -> Self {
        Self {
            reasoning: String::new(),
            text: String::new(),
            tools: BTreeMap::new(),
            // `Usage` has no `Default` impl in the models crate.
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            reasoning_started: false,
            text_started: false,
            text_index: 0,
            emitted_anything: false,
        }
    }
}

impl OpenAiProvider {
    /// Fold one chunk into `state`, emitting events as content arrives.
    /// Returns the chunk's `finish_reason`, if it carried one.
    async fn absorb_chunk(
        chunk: &ChatChunk,
        state: &mut StreamState,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<Option<StopReason>, LlmError> {
        if let Some(u) = &chunk.usage {
            state.usage.input_tokens = u.prompt_tokens;
            state.usage.output_tokens = u.completion_tokens;
        }

        let mut finish = None;

        for choice in &chunk.choices {
            // Reasoning trace, if any, streams before content on these backends.
            // Surface it as a thinking block (index 0) so the UI can show it.
            if let Some(r) = choice.delta.reasoning_trace()
                && !r.is_empty()
            {
                if !state.reasoning_started {
                    state.reasoning_started = true;
                    events
                        .emit(AgentEvent::ThinkingBlockStart(ThinkingBlockStartEvent {
                            message_id: message_id.to_string(),
                            index: 0,
                        }))
                        .await?;
                }
                state.reasoning.push_str(r);
                state.emitted_anything = true;
                events
                    .emit(AgentEvent::ThinkingChunk(ThinkingChunkEvent {
                        message_id: message_id.to_string(),
                        index: 0,
                        text: r.to_string(),
                    }))
                    .await?;
            }

            if let Some(c) = &choice.delta.content
                && !c.is_empty()
            {
                if !state.text_started {
                    state.text_started = true;
                    // A thinking block, if present, is block 0; text follows it.
                    state.text_index = u32::from(state.reasoning_started);
                    events
                        .emit(AgentEvent::TextBlockStart(TextBlockStartEvent {
                            message_id: message_id.to_string(),
                            index: state.text_index,
                        }))
                        .await?;
                }
                state.text.push_str(c);
                state.emitted_anything = true;
                events
                    .emit(AgentEvent::TextChunk(TextChunkEvent {
                        message_id: message_id.to_string(),
                        index: state.text_index,
                        text: c.clone(),
                    }))
                    .await?;
            }

            for tc in choice.delta.tool_calls.iter().flatten() {
                let idx = u32::try_from(tc.index).unwrap_or(0);
                let acc = state.tools.entry(tc.index).or_default();
                if let Some(id) = &tc.id {
                    acc.id.clone_from(id);
                }
                if let Some(f) = &tc.function {
                    if let Some(n) = &f.name {
                        acc.name.clone_from(n);
                    }
                    if let Some(a) = &f.arguments {
                        acc.args.push_str(a);
                    }
                }
                let should_start = !acc.started && !acc.id.is_empty() && !acc.name.is_empty();
                if should_start {
                    acc.started = true;
                    let (id, name) = (acc.id.clone(), acc.name.clone());
                    state.emitted_anything = true;
                    events
                        .emit(AgentEvent::ToolCallStart(ToolCallStartEvent {
                            message_id: message_id.to_string(),
                            index: idx,
                            tool_call_id: id,
                            name,
                        }))
                        .await?;
                }
                if let Some(f) = &tc.function
                    && let Some(a) = &f.arguments
                    && !a.is_empty()
                    && !acc.id.is_empty()
                {
                    let tool_call_id = acc.id.clone();
                    events
                        .emit(AgentEvent::ToolCallInputDelta(ToolCallInputDeltaEvent {
                            message_id: message_id.to_string(),
                            index: idx,
                            tool_call_id,
                            delta: a.clone(),
                        }))
                        .await?;
                }
            }

            if let Some(fr) = &choice.finish_reason {
                finish = Some(match fr.as_str() {
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                });
            }
        }

        Ok(finish)
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // The OpenAI schema defines `tool_choice`, and OpenRouter/DeepSeek/vLLM
        // honor it. Ollama and llama.cpp are inconsistent; when that bites, this
        // becomes configurable rather than hardcoded.
        ProviderCapabilities {
            supports_tool_choice: true,
        }
    }

    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        let body = self.build_body(&request);
        let mut last_error: Option<LlmError> = None;

        'retry: for attempt in 0..=MAX_STREAM_RETRIES {
            if attempt > 0 {
                let delay = self.retry_base_secs * 2u64.pow(attempt - 1);
                tracing::warn!(attempt, delay_secs = delay, "OpenAI retry");
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }

            let mut req = self.http.post(self.endpoint()).json(&body);
            if let Some(k) = &self.api_key {
                req = req.bearer_auth(k.expose());
            }

            let mut state = StreamState::default();
            let mut stop_reason = StopReason::EndTurn;
            let mut es = EventSource::new(req).map_err(io_err)?;

            while let Some(ev) = es.next().await {
                match ev {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(m)) => {
                        if m.data.trim() == "[DONE]" {
                            break;
                        }
                        let Ok(chunk) = serde_json::from_str::<ChatChunk>(&m.data) else {
                            continue;
                        };
                        if let Some(f) =
                            Self::absorb_chunk(&chunk, &mut state, message_id, events).await?
                        {
                            stop_reason = f;
                        }
                    }
                    Err(reqwest_eventsource::Error::StreamEnded) => break,
                    Err(reqwest_eventsource::Error::InvalidStatusCode(status, resp)) => {
                        let code = status.as_u16();
                        let body_text = resp.text().await.unwrap_or_default();
                        let err = classify_status(code, &body_text);
                        es.close();
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

            let mut parts: Vec<ContentPart> = Vec::new();
            // Thinking first: it precedes text on the wire and is block 0.
            if !state.reasoning.is_empty() {
                events
                    .emit(AgentEvent::ContentBlockStop(ContentBlockStopEvent {
                        message_id: message_id.to_string(),
                        index: 0,
                    }))
                    .await?;
                parts.push(ContentPart::Thinking(ThinkingPart {
                    text: state.reasoning.clone(),
                    // No cross-provider signature exists; Anthropic-only.
                    signature: None,
                }));
            }
            if !state.text.is_empty() {
                events
                    .emit(AgentEvent::ContentBlockStop(ContentBlockStopEvent {
                        message_id: message_id.to_string(),
                        index: state.text_index,
                    }))
                    .await?;
                parts.push(ContentPart::Text(TextPart {
                    text: state.text.clone(),
                }));
            }
            for acc in state.tools.into_values() {
                let input = if acc.args.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&acc.args).unwrap_or(serde_json::Value::Null)
                };
                parts.push(ContentPart::ToolCall(ToolCallPart {
                    id: acc.id,
                    name: acc.name,
                    input,
                }));
            }

            // Some compatible backends report `stop` even when they emitted tool
            // calls. Trust the content over the label.
            if stop_reason == StopReason::EndTurn
                && parts.iter().any(|p| matches!(p, ContentPart::ToolCall(_)))
            {
                stop_reason = StopReason::ToolUse;
            }

            return Ok(CompletionResponse {
                parts,
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
            id: "m1".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
        }
    }

    fn req(history: &[Message]) -> CompletionRequest<'_> {
        CompletionRequest {
            messages: history,
            system: None,
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_tokens: Some(64),
        }
    }

    fn provider(url: &str) -> OpenAiProvider {
        OpenAiProvider::with_api_key("k")
            .unwrap()
            .with_model("mock-model")
            .with_base_url(url)
            .with_retry_delay_secs(0)
    }

    #[tokio::test]
    async fn streams_text_and_reports_end_turn() {
        let server = MockLlmServer::builder().build().await;
        server.queue_response("hello from openai");

        let history = vec![user("hi")];
        let resp = provider(&server.url())
            .complete(req(&history), "msg-1", &NullSink::new())
            .await
            .unwrap();

        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        match &resp.parts[0] {
            ContentPart::Text(t) => assert_eq!(t.text, "hello from openai"),
            other => panic!("expected text, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[tokio::test]
    async fn captures_reasoning_content_as_a_thinking_part() {
        // DeepSeek/vLLM stream a reasoning trace before the answer. It surfaces
        // as a ThinkingPart (for display) ahead of the text, and emits thinking
        // events — but is never replayed (see wire::to_wire_messages).
        let server = MockLlmServer::builder().build().await;
        server.queue_reasoning("let me think about it", "the answer is 42");

        let history = vec![user("hi")];
        let sink = NullSink::new();
        let resp = provider(&server.url())
            .complete(req(&history), "msg-1", &sink)
            .await
            .unwrap();

        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        match &resp.parts[0] {
            ContentPart::Thinking(t) => {
                assert_eq!(t.text, "let me think about it");
                assert_eq!(t.signature, None);
            }
            other => panic!(
                "expected thinking first, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        match &resp.parts[1] {
            ContentPart::Text(t) => assert_eq!(t.text, "the answer is 42"),
            other => panic!(
                "expected text second, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        assert!(
            sink.events()
                .iter()
                .any(|e| matches!(e, AgentEvent::ThinkingChunk(_))),
            "expected a ThinkingChunk event",
        );
    }

    #[tokio::test]
    async fn streams_a_tool_call_and_reports_tool_use() {
        let server = MockLlmServer::builder().build().await;
        server.queue_tool_call("echo", serde_json::json!({ "value": 42 }));

        let history = vec![user("go")];
        let resp = provider(&server.url())
            .complete(req(&history), "msg-1", &NullSink::new())
            .await
            .unwrap();

        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        match &resp.parts[0] {
            ContentPart::ToolCall(tc) => {
                assert_eq!(tc.name, "echo");
                assert_eq!(tc.input, serde_json::json!({ "value": 42 }));
            }
            other => panic!(
                "expected tool call, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    #[tokio::test]
    async fn reports_max_tokens_truncation() {
        let server = MockLlmServer::builder().build().await;
        server.queue_truncated("cut off here");

        let history = vec![user("hi")];
        let resp = provider(&server.url())
            .complete(req(&history), "msg-1", &NullSink::new())
            .await
            .unwrap();

        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
    }

    #[tokio::test]
    async fn classifies_429_as_rate_limit() {
        let server = MockLlmServer::builder().build().await;
        for _ in 0..12 {
            server.queue_error(429, "slow down");
        }

        let history = vec![user("hi")];
        let err = provider(&server.url())
            .complete(req(&history), "msg-1", &NullSink::new())
            .await
            .expect_err("expected an error");

        assert!(
            matches!(err, LlmError::RateLimit { .. }),
            "expected RateLimit, got {err:?}",
        );
    }

    #[tokio::test]
    async fn classifies_400_as_api_error_not_retryable() {
        let server = MockLlmServer::builder().build().await;
        server.queue_error(400, "bad request");

        let history = vec![user("hi")];
        let err = provider(&server.url())
            .complete(req(&history), "msg-1", &NullSink::new())
            .await
            .expect_err("expected an error");

        assert!(
            matches!(err, LlmError::ApiError { status: 400, .. }),
            "expected ApiError 400, got {err:?}",
        );
    }
}
