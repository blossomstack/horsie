//! LLM-provider adapters backed by the `async-llm` client crate.

use async_llm::openai::{
    ChatCompletionError, ChatCompletionRequest, ChatMessage, Client, FunctionCall, FunctionDef,
    StreamOptions, ToolCall as AsyncToolCall, ToolChoice as AsyncToolChoice, ToolDef,
};
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

pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const MAX_STREAM_RETRIES: u32 = 6;
const BACKOFF_BASE_SECS: u64 = 5;
const DEFAULT_READ_TIMEOUT_SECS: u64 = 120;

#[must_use]
pub fn env_base_url() -> Option<String> {
    env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
}

pub struct OpenAiProvider {
    model: String,
    api_key: Option<Secret>,
    base_url: String,
    max_tokens: Option<u32>,
    retry_delay: Duration,
    read_timeout: Duration,
    thinking_dialect: ThinkingDialect,
    forced_tools_disable_thinking: bool,
}

impl OpenAiProvider {
    fn build(api_key: Option<Secret>) -> Result<Self, LlmError> {
        let provider = Self {
            model: DEFAULT_MODEL.to_string(),
            api_key,
            base_url: env_base_url().unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            max_tokens: None,
            retry_delay: Duration::from_secs(BACKOFF_BASE_SECS),
            read_timeout: Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS),
            thinking_dialect: ThinkingDialect::NoControl,
            forced_tools_disable_thinking: false,
        };
        provider.client()?;
        Ok(provider)
    }

    /// Reads `OPENAI_API_KEY`; an absent key supports local compatible backends.
    pub fn new() -> Result<Self, LlmError> {
        let api_key = env::var("OPENAI_API_KEY")
            .ok()
            .filter(|value| !value.is_empty())
            .map(Secret::from);
        Self::build(api_key)
    }

    pub fn with_api_key(api_key: impl Into<Secret>) -> Result<Self, LlmError> {
        Self::build(Some(api_key.into()))
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

    #[must_use]
    pub fn with_forced_tools_disable_thinking(mut self, enabled: bool) -> Self {
        self.forced_tools_disable_thinking = enabled;
        self
    }

    fn client(&self) -> Result<Client, LlmError> {
        let mut builder = Client::builder()
            .base_url(self.base_url.as_str())
            .read_timeout(self.read_timeout)
            .max_retries(MAX_STREAM_RETRIES)
            .retry_delay(self.retry_delay);
        if let Some(api_key) = &self.api_key {
            builder = builder.api_key(api_key.expose().to_owned());
        }
        builder.build().map_err(map_error)
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> ChatCompletionRequest {
        let mut messages = Vec::new();
        if let Some(system) = &request.system {
            messages.push(ChatMessage::system(system));
        }
        for message in request.messages {
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            for part in &message.parts {
                match part {
                    ContentPart::Text(part) => text.push_str(&part.text),
                    ContentPart::ToolCall(call) => tool_calls.push(AsyncToolCall {
                        id: call.id.clone(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: call.name.clone(),
                            arguments: serde_json::to_string(&call.input).unwrap_or_default(),
                        },
                    }),
                    ContentPart::ToolResult(result) => {
                        messages.push(ChatMessage::tool(&result.tool_call_id, &result.output));
                    }
                    ContentPart::Thinking(_) => {}
                    ContentPart::SubAgentResult(result) => text.push_str(&result.to_wire_text()),
                }
            }
            if text.is_empty() && tool_calls.is_empty() {
                continue;
            }
            let role = match message.role {
                Role::Assistant => "assistant",
                Role::User | Role::Tool => "user",
            };
            let mut wire_message = ChatMessage::new(role, (!text.is_empty()).then_some(text));
            if !tool_calls.is_empty() {
                wire_message.tool_calls = Some(tool_calls);
            }
            messages.push(wire_message);
        }

        let tools: Vec<ToolDef> = request
            .tools
            .iter()
            .map(|tool| {
                ToolDef::function(FunctionDef {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.input_schema.clone(),
                })
            })
            .collect();
        let tool_choice = if tools.is_empty() {
            None
        } else {
            match &request.tool_choice {
                ToolChoice::Auto => None,
                ToolChoice::Any => Some(AsyncToolChoice::Required),
                ToolChoice::Required(name) => {
                    Some(AsyncToolChoice::Function { name: name.clone() })
                }
            }
        };
        let reasoning_effort = if self.forced_tools_disable_thinking && tool_choice.is_some() {
            Some("none".to_string())
        } else {
            match (self.thinking_dialect, request.thinking_effort) {
                (ThinkingDialect::OpenAiEffort, Some(effort)) => Some(effort.as_str().to_string()),
                _ => None,
            }
        };

        ChatCompletionRequest {
            model: self.model.clone(),
            messages,
            stream: true,
            max_tokens: self
                .max_tokens
                .or(request.max_tokens)
                .or(Some(DEFAULT_MAX_TOKENS)),
            tools,
            tool_choice,
            reasoning_effort,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
        }
    }
}

fn map_error(error: ChatCompletionError) -> LlmError {
    match error {
        ChatCompletionError::Network(error) => LlmError::Network(Box::new(error)),
        ChatCompletionError::RateLimited { .. } => LlmError::RateLimit { retry_after: None },
        ChatCompletionError::Overloaded { .. } => LlmError::Overloaded,
        ChatCompletionError::Api { status, body } => LlmError::ApiError {
            status,
            message: body,
        },
        ChatCompletionError::IncompleteStream | ChatCompletionError::Stream(_) => {
            LlmError::Network(Box::new(std::io::Error::other(error.to_string())))
        }
    }
}

fn parse_tool_input(raw: &str, tool: &str) -> Result<serde_json::Value, LlmError> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|error| LlmError::ApiError {
        status: 502,
        message: format!(
            "tool call '{tool}' had unparseable input JSON ({error}); {} byte(s) received, likely a truncated stream",
            raw.len()
        ),
    })
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

struct StreamState {
    reasoning: String,
    text: String,
    usage: Usage,
    reasoning_started: bool,
    text_started: bool,
    text_index: u32,
    tools: BTreeMap<usize, ToolAcc>,
}

impl Default for StreamState {
    fn default() -> Self {
        Self {
            reasoning: String::new(),
            text: String::new(),
            usage: Usage::without_cache(0, 0),
            reasoning_started: false,
            text_started: false,
            text_index: 0,
            tools: BTreeMap::new(),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
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
            .client()?
            .stream(self.build_body(&request))
            .await
            .map_err(map_error)?;
        let mut state = StreamState::default();
        let mut stop_reason = StopReason::EndTurn;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_error)?;
            if let Some(usage) = chunk.usage {
                state.usage.input_tokens = usage.prompt_tokens;
                state.usage.output_tokens = usage.completion_tokens;
                state.usage.cache_read_tokens = usage.cached_tokens();
            }
            for choice in chunk.choices {
                if let Some(reasoning) = choice
                    .delta
                    .reasoning_trace()
                    .filter(|reasoning| !reasoning.is_empty())
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
                    state.reasoning.push_str(reasoning);
                    events
                        .emit(AgentEvent::ThinkingChunk(ThinkingChunkEvent {
                            message_id: message_id.to_string(),
                            index: 0,
                            text: reasoning.to_string(),
                        }))
                        .await?;
                }
                if let Some(content) = choice.delta.content.filter(|content| !content.is_empty()) {
                    if !state.text_started {
                        state.text_started = true;
                        state.text_index = u32::from(state.reasoning_started);
                        events
                            .emit(AgentEvent::TextBlockStart(TextBlockStartEvent {
                                message_id: message_id.to_string(),
                                index: state.text_index,
                            }))
                            .await?;
                    }
                    state.text.push_str(&content);
                    events
                        .emit(AgentEvent::TextChunk(TextChunkEvent {
                            message_id: message_id.to_string(),
                            index: state.text_index,
                            text: content,
                        }))
                        .await?;
                }
                for tool_call in choice.delta.tool_calls.iter().flatten() {
                    let accumulator = state.tools.entry(tool_call.index).or_default();
                    if let Some(id) = &tool_call.id {
                        accumulator.id.clone_from(id);
                    }
                    if let Some(function) = &tool_call.function {
                        if let Some(name) = &function.name {
                            accumulator.name.clone_from(name);
                        }
                        if let Some(arguments) = &function.arguments {
                            accumulator.arguments.push_str(arguments);
                        }
                    }
                    if !accumulator.started
                        && !accumulator.id.is_empty()
                        && !accumulator.name.is_empty()
                    {
                        accumulator.started = true;
                        events
                            .emit(AgentEvent::ToolCallStart(ToolCallStartEvent {
                                message_id: message_id.to_string(),
                                index: u32::try_from(tool_call.index).unwrap_or(0),
                                tool_call_id: accumulator.id.clone(),
                                name: accumulator.name.clone(),
                            }))
                            .await?;
                    }
                    if let Some(arguments) = tool_call
                        .function
                        .as_ref()
                        .and_then(|function| function.arguments.as_ref())
                        .filter(|arguments| !arguments.is_empty())
                        && !accumulator.id.is_empty()
                    {
                        events
                            .emit(AgentEvent::ToolCallInputDelta(ToolCallInputDeltaEvent {
                                message_id: message_id.to_string(),
                                index: u32::try_from(tool_call.index).unwrap_or(0),
                                tool_call_id: accumulator.id.clone(),
                                delta: arguments.clone(),
                            }))
                            .await?;
                    }
                }
                if let Some(finish_reason) = choice.finish_reason {
                    stop_reason = match finish_reason.as_str() {
                        "length" => StopReason::MaxTokens,
                        _ => StopReason::EndTurn,
                    };
                }
            }
        }

        let mut parts = Vec::new();
        if !state.reasoning.is_empty() {
            events
                .emit(AgentEvent::ContentBlockStop(ContentBlockStopEvent {
                    message_id: message_id.to_string(),
                    index: 0,
                }))
                .await?;
            parts.push(ContentPart::Thinking(ThinkingPart {
                text: state.reasoning,
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
            parts.push(ContentPart::Text(TextPart { text: state.text }));
        }
        let has_tools = !state.tools.is_empty();
        for tool in state.tools.into_values() {
            parts.push(ContentPart::ToolCall(ToolCallPart {
                id: tool.id,
                name: tool.name.clone(),
                input: parse_tool_input(&tool.arguments, &tool.name)?,
            }));
        }
        if stop_reason == StopReason::EndTurn && has_tools {
            stop_reason = StopReason::ToolUse;
        }

        Ok(CompletionResponse {
            parts,
            stop_reason,
            usage: state.usage,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_agentcore::{ToolChoice, ToolSpec};
    use horsie_models::agent::{Message, Role, ThinkingPart, ToolCallPart, ToolResultPart};

    #[test]
    fn maps_assistant_tool_calls_and_tool_results_to_openai_messages() {
        let provider = OpenAiProvider::with_api_key("test-key").unwrap();
        let messages = vec![
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "assistant-1".into(),
                role: Role::Assistant,
                parts: vec![
                    ContentPart::Thinking(ThinkingPart {
                        text: "private reasoning".into(),
                        signature: None,
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "call-1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"value": 42}),
                    }),
                ],
            },
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "tool-1".into(),
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "call-1".into(),
                    output: "42".into(),
                    is_error: false,
                })],
            },
        ];
        let request = CompletionRequest {
            messages: &messages,
            system: Some("Be brief.".into()),
            tools: vec![ToolSpec {
                name: "echo".into(),
                description: "Echo an input.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            tool_choice: ToolChoice::Required("echo".into()),
            max_tokens: None,
            thinking_effort: None,
            conversation_id: "test-conversation",
        };

        let body = provider.build_body(&request);

        assert_eq!(body.messages.len(), 3);
        assert_eq!(body.messages[0], ChatMessage::system("Be brief."));
        assert_eq!(body.messages[1].role, "assistant");
        assert_eq!(body.messages[1].content, None);
        assert_eq!(
            body.messages[1]
                .tool_calls
                .as_ref()
                .expect("assistant tool call"),
            &vec![async_llm::openai::ToolCall {
                id: "call-1".into(),
                kind: "function".into(),
                function: async_llm::openai::FunctionCall {
                    name: "echo".into(),
                    arguments: r#"{"value":42}"#.into(),
                },
            }]
        );
        assert_eq!(body.messages[2], ChatMessage::tool("call-1", "42"));
        assert_eq!(
            body.tool_choice,
            Some(AsyncToolChoice::Function {
                name: "echo".into()
            })
        );
        assert_eq!(
            body.stream_options,
            Some(StreamOptions {
                include_usage: true
            })
        );
    }

    #[test]
    fn disables_openai_thinking_for_pinned_tools_when_configured() {
        let provider = OpenAiProvider::with_api_key("test-key")
            .unwrap()
            .with_thinking_dialect(horsie_agentcore::ThinkingDialect::OpenAiEffort)
            .with_forced_tools_disable_thinking(true);
        let messages = Vec::new();
        let request = CompletionRequest {
            messages: &messages,
            system: None,
            tools: vec![ToolSpec {
                name: "echo".into(),
                description: "Echo an input.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            tool_choice: ToolChoice::Required("echo".into()),
            max_tokens: None,
            thinking_effort: horsie_agentcore::ThinkingEffort::parse("high"),
            conversation_id: "test-conversation",
        };

        assert_eq!(
            provider.build_body(&request).reasoning_effort.as_deref(),
            Some("none")
        );
    }

    #[test]
    fn auto_tools_keep_openai_thinking_enabled_when_forced_tools_disable_it() {
        let provider = OpenAiProvider::with_api_key("test-key")
            .unwrap()
            .with_thinking_dialect(horsie_agentcore::ThinkingDialect::OpenAiEffort)
            .with_forced_tools_disable_thinking(true);
        let messages = Vec::new();
        let request = CompletionRequest {
            messages: &messages,
            system: None,
            tools: vec![ToolSpec {
                name: "echo".into(),
                description: "Echo an input.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            tool_choice: ToolChoice::Auto,
            max_tokens: None,
            thinking_effort: horsie_agentcore::ThinkingEffort::parse("high"),
            conversation_id: "test-conversation",
        };

        let body = provider.build_body(&request);

        assert_eq!(body.tool_choice, None);
        assert_eq!(body.reasoning_effort.as_deref(), Some("high"));
    }
}

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

pub const DEFAULT_RESPONSES_MODEL: &str = "gpt-5";
pub const DEFAULT_RESPONSES_MAX_TOKENS: u32 = 32_768;

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
            model: DEFAULT_RESPONSES_MODEL.to_string(),
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
                .or(Some(DEFAULT_RESPONSES_MAX_TOKENS));
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
