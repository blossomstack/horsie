//! OpenAI's Responses API, adapted to horsie's `LlmProvider`.
//!
//! Separate from [`crate::openai`] because it is a different wire protocol on
//! the same host — and because it is the only one a ChatGPT subscription can be
//! spent through.

use crate::openai::DEFAULT_BASE_URL;
use crate::{BACKOFF_BASE_SECS, DEFAULT_READ_TIMEOUT_SECS, MAX_STREAM_RETRIES, parse_tool_input};
use async_llm::responses::{
    Client as ResponsesClient, FunctionTool as ResponsesFunctionTool,
    ReasoningControl as ResponsesReasoningControl, ResponsesError, ResponsesRequest,
    ResponsesStreamEvent,
};
use async_trait::async_trait;
use horsie_agentcore::{
    AgentEvent, CompletionRequest, CompletionResponse, ContentBlockStopEvent, ContentPart,
    EventSink, LlmError, LlmProvider, Secret, StopReason, TextBlockStartEvent, TextChunkEvent,
    TextPart, ThinkingBlockStartEvent, ThinkingChunkEvent, ThinkingDialect, ThinkingPart,
    ToolCallInputDeltaEvent, ToolCallPart, ToolCallStartEvent, ToolChoice, Usage,
};
use horsie_models::agent::Role;
use std::{collections::BTreeMap, env, sync::Arc, time::Duration};
use tokio_stream::StreamExt;

/// ChatGPT device-login and refreshable-token types used by the Responses
/// client. They are re-exported here so server consumers can move adapters
/// without reaching into `async-llm`.
pub mod chatgpt {
    /// What a [`TokenStore`] implementation has to return, re-exported so a
    /// caller needs one import rather than two.
    pub use async_llm::responses::ResponsesError;
    pub use async_llm::responses::chatgpt::*;
}

/// OpenAI's own Codex OAuth client. Third parties cannot register one — there
/// is no allocation mechanism — so this constant is the only usable client id.
pub const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// How we identify ourselves. opencode sends its own name and is not blocked;
/// impersonating `codex_cli_rs` would be a lie we do not need to tell.
pub const CHATGPT_ORIGINATOR: &str = "horsie";

/// How horsie identifies itself to OpenAI's ChatGPT auth and Codex endpoints.
/// Every caller that logs in, refreshes, or spends a subscription must use the
/// same one, so it is built here rather than assembled at each call site.
#[must_use]
pub fn chatgpt_auth() -> chatgpt::ChatGptAuth {
    chatgpt::ChatGptAuth::new(CHATGPT_CLIENT_ID).with_originator(CHATGPT_ORIGINATOR)
}

pub const DEFAULT_MODEL: &str = "gpt-5";
pub const DEFAULT_MAX_TOKENS: u32 = 32_768;

#[derive(Clone)]
enum ResponsesCredential {
    ApiKey(Secret),
    ChatGpt(Arc<chatgpt::ChatGptTokens>),
}

/// An OpenAI Responses-API adapter backed by `async-llm`.
pub struct ResponsesProvider {
    model: String,
    credential: ResponsesCredential,
    base_url: String,
    max_tokens: Option<u32>,
    retry_delay: Duration,
    read_timeout: Duration,
    thinking_dialect: ThinkingDialect,
}

impl ResponsesProvider {
    fn build(credential: ResponsesCredential) -> Result<Self, LlmError> {
        Ok(Self {
            model: DEFAULT_MODEL.to_string(),
            credential,
            base_url: DEFAULT_BASE_URL.to_string(),
            max_tokens: None,
            retry_delay: Duration::from_secs(BACKOFF_BASE_SECS),
            read_timeout: Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS),
            thinking_dialect: ThinkingDialect::NoControl,
        })
    }

    /// Reads `OPENAI_API_KEY`; an absent key supports local compatible backends.
    pub fn new() -> Result<Self, LlmError> {
        let api_key = env::var("OPENAI_API_KEY")
            .ok()
            .filter(|value| !value.is_empty())
            .map(Secret::from)
            .unwrap_or_else(|| Secret::from(""));
        Self::build(ResponsesCredential::ApiKey(api_key))
    }

    pub fn with_api_key(api_key: impl Into<Secret>) -> Result<Self, LlmError> {
        Self::build(ResponsesCredential::ApiKey(api_key.into()))
    }

    /// A provider that spends a ChatGPT subscription.
    pub fn with_chatgpt(tokens: Arc<chatgpt::ChatGptTokens>) -> Result<Self, LlmError> {
        let mut provider = Self::build(ResponsesCredential::ChatGpt(tokens))?;
        provider.base_url = "https://chatgpt.com".to_string();
        Ok(provider)
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    #[must_use]
    pub fn with_retry_delay_secs(mut self, seconds: u64) -> Self {
        self.retry_delay = Duration::from_secs(seconds);
        self
    }

    #[must_use]
    pub fn with_read_timeout_secs(mut self, seconds: u64) -> Self {
        self.read_timeout = Duration::from_secs(seconds);
        self
    }

    #[must_use]
    pub fn with_thinking_dialect(mut self, thinking_dialect: ThinkingDialect) -> Self {
        self.thinking_dialect = thinking_dialect;
        self
    }

    fn client(&self) -> ResponsesClient {
        match &self.credential {
            ResponsesCredential::ApiKey(api_key) => {
                ResponsesClient::with_api_key(api_key.expose().to_owned())
            }
            ResponsesCredential::ChatGpt(tokens) => ResponsesClient::with_chatgpt(tokens.clone()),
        }
        .with_base_url(self.base_url.clone())
        .max_retries(MAX_STREAM_RETRIES)
        .retry_delay(self.retry_delay)
        .read_timeout(self.read_timeout)
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> ResponsesRequest {
        let mut body =
            ResponsesRequest::new(self.model.clone(), responses_input_items(request.messages));
        body.instructions = request.system.clone();
        body.tools = request
            .tools
            .iter()
            .map(|tool| {
                ResponsesFunctionTool::new(
                    tool.name.clone(),
                    tool.description.clone(),
                    tool.input_schema.clone(),
                )
            })
            .collect();
        body.tool_choice = if body.tools.is_empty() {
            None
        } else {
            match &request.tool_choice {
                ToolChoice::Auto => None,
                ToolChoice::Any => Some(serde_json::json!("required")),
                ToolChoice::Required(name) => Some(serde_json::json!({
                    "type": "function",
                    "name": name,
                })),
            }
        };
        body.reasoning = match (self.thinking_dialect, request.thinking_effort) {
            (ThinkingDialect::OpenAiEffort, Some(effort)) => Some(ResponsesReasoningControl {
                effort: effort.as_str().to_string(),
                summary: Some("auto".to_string()),
            }),
            _ => None,
        };
        if matches!(self.credential, ResponsesCredential::ApiKey(_)) {
            body.max_output_tokens = self
                .max_tokens
                .or(request.max_tokens)
                .or(Some(DEFAULT_MAX_TOKENS));
            body.prompt_cache_key = Some(format!("horsie-{}", request.conversation_id));
        }
        body
    }
}

fn map_responses_error(error: ResponsesError) -> LlmError {
    match error {
        ResponsesError::Network(error) => LlmError::Network(Box::new(error)),
        ResponsesError::RateLimited { .. } => LlmError::RateLimit { retry_after: None },
        ResponsesError::Overloaded { .. } => LlmError::Overloaded,
        ResponsesError::Api { status, body } => LlmError::ApiError {
            status,
            message: body,
        },
        ResponsesError::Authentication(message) => LlmError::ApiError {
            status: 401,
            message,
        },
        ResponsesError::IncompleteStream | ResponsesError::Stream(_) => {
            LlmError::Network(Box::new(std::io::Error::other(error.to_string())))
        }
    }
}

#[derive(Default)]
struct ResponsesItemAcc {
    kind: String,
    tool_call_id: String,
    name: String,
}

struct ResponsesStreamState {
    items: BTreeMap<usize, ResponsesItemAcc>,
    parts: Vec<ContentPart>,
    usage: Usage,
}

impl Default for ResponsesStreamState {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            parts: Vec::new(),
            usage: Usage::without_cache(0, 0),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ResponsesReasoningRef {
    id: String,
    #[serde(rename = "enc")]
    encrypted: String,
}

impl ResponsesReasoningRef {
    fn to_signature(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }
}

fn responses_input_items(messages: &[horsie_models::agent::Message]) -> Vec<serde_json::Value> {
    let mut input = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::Assistant => "assistant",
            Role::User | Role::Tool => "user",
        };
        let mut text = String::new();
        let flush_text = |text: &mut String, input: &mut Vec<serde_json::Value>| {
            if !text.is_empty() {
                let content_type = if role == "assistant" {
                    "output_text"
                } else {
                    "input_text"
                };
                input.push(serde_json::json!({
                    "type": "message",
                    "role": role,
                    "content": [{ "type": content_type, "text": text }],
                }));
                text.clear();
            }
        };

        for part in &message.parts {
            match part {
                ContentPart::Text(part) => text.push_str(&part.text),
                ContentPart::SubAgentResult(part) => text.push_str(&part.to_wire_text()),
                ContentPart::Artifact(a) => text.push_str(&a.artifact.omitted_text()),
                ContentPart::Thinking(part) => {
                    if let Some(reasoning) = part.signature.as_deref().and_then(|signature| {
                        serde_json::from_str::<ResponsesReasoningRef>(signature).ok()
                    }) {
                        flush_text(&mut text, &mut input);
                        input.push(serde_json::json!({
                            "type": "reasoning",
                            "id": reasoning.id,
                            "encrypted_content": reasoning.encrypted,
                            "summary": [],
                        }));
                    }
                }
                ContentPart::ToolCall(part) => {
                    flush_text(&mut text, &mut input);
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": part.id,
                        "name": part.name,
                        "arguments": serde_json::to_string(&part.input).unwrap_or_default(),
                    }));
                }
                ContentPart::ToolResult(part) => {
                    flush_text(&mut text, &mut input);
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": part.tool_call_id,
                        "output": part.output,
                    }));
                }
            }
        }
        flush_text(&mut text, &mut input);
    }
    input
}

impl ResponsesProvider {
    async fn absorb_event(
        event: ResponsesStreamEvent,
        state: &mut ResponsesStreamState,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<Option<StopReason>, LlmError> {
        match event {
            ResponsesStreamEvent::OutputItemAdded { output_index, item } => {
                let index = u32::try_from(output_index).unwrap_or(0);
                match item.kind.as_str() {
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
                        let tool_call_id = item.call_id.unwrap_or_default();
                        let name = item.name.unwrap_or_default();
                        events
                            .emit(AgentEvent::ToolCallStart(ToolCallStartEvent {
                                message_id: message_id.to_string(),
                                index,
                                tool_call_id: tool_call_id.clone(),
                                name: name.clone(),
                            }))
                            .await?;
                        state.items.insert(
                            output_index,
                            ResponsesItemAcc {
                                kind: item.kind,
                                tool_call_id,
                                name,
                            },
                        );
                        return Ok(None);
                    }
                    _ => {}
                }
                state.items.insert(
                    output_index,
                    ResponsesItemAcc {
                        kind: item.kind,
                        ..ResponsesItemAcc::default()
                    },
                );
            }
            ResponsesStreamEvent::OutputTextDelta {
                output_index,
                delta,
                ..
            } if !delta.is_empty() => {
                events
                    .emit(AgentEvent::TextChunk(TextChunkEvent {
                        message_id: message_id.to_string(),
                        index: u32::try_from(output_index).unwrap_or(0),
                        text: delta,
                    }))
                    .await?;
            }
            ResponsesStreamEvent::ReasoningSummaryTextDelta {
                output_index,
                delta,
                ..
            }
            | ResponsesStreamEvent::ReasoningTextDelta {
                output_index,
                delta,
                ..
            } if !delta.is_empty() => {
                events
                    .emit(AgentEvent::ThinkingChunk(ThinkingChunkEvent {
                        message_id: message_id.to_string(),
                        index: u32::try_from(output_index).unwrap_or(0),
                        text: delta,
                    }))
                    .await?;
            }
            ResponsesStreamEvent::FunctionCallArgumentsDelta {
                output_index,
                call_id,
                delta,
                ..
            } if !delta.is_empty() => {
                if let Some(item) = state.items.get(&output_index) {
                    events
                        .emit(AgentEvent::ToolCallInputDelta(ToolCallInputDeltaEvent {
                            message_id: message_id.to_string(),
                            index: u32::try_from(output_index).unwrap_or(0),
                            tool_call_id: call_id.unwrap_or_else(|| item.tool_call_id.clone()),
                            delta,
                        }))
                        .await?;
                }
            }
            ResponsesStreamEvent::OutputItemDone { output_index, item } => {
                let index = u32::try_from(output_index).unwrap_or(0);
                let accumulator = state.items.remove(&output_index).unwrap_or_default();
                let kind = if !item.kind.is_empty() {
                    item.kind.as_str()
                } else {
                    accumulator.kind.as_str()
                };
                match kind {
                    "message" => {
                        let text = item
                            .content
                            .iter()
                            .filter_map(|content| content.text.as_deref())
                            .collect::<String>();
                        if !text.is_empty() {
                            state.parts.push(ContentPart::Text(TextPart { text }));
                        }
                    }
                    "reasoning" => {
                        let text = item
                            .summary
                            .iter()
                            .filter_map(|content| content.text.as_deref())
                            .collect::<String>();
                        let signature = item.encrypted_content.and_then(|encrypted| {
                            ResponsesReasoningRef {
                                id: item.id.unwrap_or_default(),
                                encrypted,
                            }
                            .to_signature()
                        });
                        if !text.is_empty() || signature.is_some() {
                            state
                                .parts
                                .push(ContentPart::Thinking(ThinkingPart { text, signature }));
                        }
                    }
                    "function_call" => {
                        let name = item.name.unwrap_or(accumulator.name);
                        let input =
                            parse_tool_input(item.arguments.as_deref().unwrap_or_default(), &name)?;
                        state.parts.push(ContentPart::ToolCall(ToolCallPart {
                            id: item.call_id.unwrap_or(accumulator.tool_call_id),
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
            ResponsesStreamEvent::Completed { response } => {
                if let Some(usage) = response.usage {
                    state.usage.input_tokens = usage.input_tokens;
                    state.usage.output_tokens = usage.output_tokens;
                }
                return Ok(Some(responses_stop_reason(state, false)));
            }
            ResponsesStreamEvent::Incomplete { response } => {
                if let Some(usage) = response.usage {
                    state.usage.input_tokens = usage.input_tokens;
                    state.usage.output_tokens = usage.output_tokens;
                }
                return Ok(Some(responses_stop_reason(state, true)));
            }
            ResponsesStreamEvent::Failed { response } => {
                return Err(LlmError::ApiError {
                    status: 502,
                    message: response
                        .error
                        .and_then(|error| error.message)
                        .unwrap_or_else(|| "response failed".to_string()),
                });
            }
            ResponsesStreamEvent::Error { message, .. } => {
                return Err(LlmError::ApiError {
                    status: 502,
                    message,
                });
            }
            ResponsesStreamEvent::OutputTextDelta { .. }
            | ResponsesStreamEvent::OutputTextDone { .. }
            | ResponsesStreamEvent::FunctionCallArgumentsDelta { .. }
            | ResponsesStreamEvent::FunctionCallArgumentsDone { .. }
            | ResponsesStreamEvent::ReasoningSummaryTextDelta { .. }
            | ResponsesStreamEvent::ReasoningEncryptedContent { .. }
            | ResponsesStreamEvent::ReasoningTextDelta { .. }
            | ResponsesStreamEvent::Other { .. } => {}
        }
        Ok(None)
    }
}

fn responses_stop_reason(state: &ResponsesStreamState, incomplete: bool) -> StopReason {
    if incomplete {
        StopReason::MaxTokens
    } else if state
        .parts
        .iter()
        .any(|part| matches!(part, ContentPart::ToolCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
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
        let mut stream = self
            .client()
            .stream(self.build_body(&request))
            .await
            .map_err(map_responses_error)?;
        let mut state = ResponsesStreamState::default();
        while let Some(event) = stream.next().await {
            let event = event.map_err(map_responses_error)?;
            if let Some(stop_reason) =
                Self::absorb_event(event, &mut state, message_id, events).await?
            {
                return Ok(CompletionResponse {
                    parts: state.parts,
                    stop_reason,
                    usage: state.usage,
                });
            }
        }
        Err(LlmError::Network(Box::new(std::io::Error::other(
            "Responses stream ended without a terminal frame",
        ))))
    }
}
