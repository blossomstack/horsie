//! OpenAI Chat Completions, adapted to horsie's `LlmProvider`.

use crate::{BACKOFF_BASE_SECS, DEFAULT_READ_TIMEOUT_SECS, MAX_STREAM_RETRIES, parse_tool_input};
use async_llm::openai::{
    ChatCompletionError, ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, Client,
    FunctionCall, FunctionDef, StreamOptions, ToolCall as AsyncToolCall,
    ToolChoice as AsyncToolChoice, ToolDef,
};
use async_trait::async_trait;
use horsie_agentcore::{
    AgentEvent, ArtifactBytes, CompletionRequest, CompletionResponse, ContentBlockStopEvent,
    ContentPart, EventSink, LlmError, LlmProvider, Secret, StopReason, TextBlockStartEvent,
    TextChunkEvent, TextPart, ThinkingBlockStartEvent, ThinkingChunkEvent, ThinkingDialect,
    ThinkingPart, ToolCallInputDeltaEvent, ToolCallPart, ToolCallStartEvent, ToolChoice, Usage,
};
use horsie_models::agent::Role;
use std::{collections::BTreeMap, env, time::Duration};
use tokio_stream::StreamExt;

pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;
/// A bare host: the client appends `/v1/chat/completions` itself. Shared with
/// [`crate::responses`], which appends `/v1/responses` to the same origin.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

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

/// An artifact as an OpenAI content part, or the text to use instead when it
/// was not hydrated — a model with no vision capability.
fn media_part(
    artifact: &horsie_models::agent::ArtifactRef,
    artifacts: &ArtifactBytes,
) -> Result<ChatContentPart, String> {
    let Some(data) = artifacts.get(&artifact.id) else {
        return Err(artifact.omitted_text());
    };
    Ok(match artifact.kind {
        horsie_models::agent::ArtifactKind::Image(_) => {
            ChatContentPart::image_base64(&artifact.media_type, data)
        }
        horsie_models::agent::ArtifactKind::Document(_) => ChatContentPart::file_base64(
            // The API parses inline file data by filename, so one is required
            // even when the caller never supplied it.
            artifact.filename.as_deref().unwrap_or("document.pdf"),
            &artifact.media_type,
            data,
        ),
    })
}

/// Emit a tool result's images as a user message.
///
/// Chat Completions rejects an image inside a `tool` message, so an image a
/// tool produced cannot travel with its own result. It follows as a user turn
/// instead, which is the only place the API accepts one. Anthropic has no such
/// restriction, which is why only this provider needs the extra message.
fn flush_deferred(messages: &mut Vec<ChatMessage>, deferred: Vec<ChatContentPart>) {
    if deferred.is_empty() {
        return;
    }
    let mut parts = vec![ChatContentPart::text(
        "The tool call above returned the following attachment(s).",
    )];
    parts.extend(deferred);
    messages.push(ChatMessage::parts("user", parts));
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
            messages.push(ChatMessage::system(system.as_str()));
        }
        for message in request.messages {
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            // Media parts for this message, in order. Kept separate from `text`
            // because a multi-part message is a different wire shape, and a
            // message with no media must stay a bare string.
            let mut media: Vec<ChatContentPart> = Vec::new();
            // Artifacts a tool result carried. A `tool`-role message may not
            // hold an image in Chat Completions, so these are re-announced in a
            // user message that follows the tool result — see `flush_deferred`.
            let mut deferred: Vec<ChatContentPart> = Vec::new();
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
                        messages.push(ChatMessage::tool(
                            &result.tool_call_id,
                            result.output.as_str(),
                        ));
                        for artifact in &result.artifacts {
                            match media_part(artifact, request.artifacts) {
                                Ok(part) => deferred.push(part),
                                Err(note) => {
                                    messages.push(ChatMessage::tool(
                                        &result.tool_call_id,
                                        note.as_str(),
                                    ));
                                }
                            }
                        }
                    }
                    ContentPart::Thinking(_) => {}
                    ContentPart::SubAgentResult(result) => text.push_str(&result.to_wire_text()),
                    ContentPart::Artifact(a) => match media_part(&a.artifact, request.artifacts) {
                        Ok(part) => media.push(part),
                        Err(note) => text.push_str(&note),
                    },
                }
            }
            if text.is_empty() && tool_calls.is_empty() && media.is_empty() {
                flush_deferred(&mut messages, deferred);
                continue;
            }
            let role = match message.role {
                Role::Assistant => "assistant",
                Role::User | Role::Tool => "user",
            };
            let content = if media.is_empty() {
                (!text.is_empty()).then_some(ChatContent::Text(text))
            } else {
                // Text first, then the media, matching the order they were
                // written in.
                let mut parts = Vec::with_capacity(media.len() + 1);
                if !text.is_empty() {
                    parts.push(ChatContentPart::text(text));
                }
                parts.extend(media);
                Some(ChatContent::Parts(parts))
            };
            let mut wire_message = ChatMessage::new(role, content);
            if !tool_calls.is_empty() {
                wire_message.tool_calls = Some(tool_calls);
            }
            messages.push(wire_message);
            flush_deferred(&mut messages, deferred);
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
                    artifacts: Vec::new(),
                })],
            },
        ];
        let request = CompletionRequest {
            messages: &messages,
            artifacts: horsie_agentcore::ArtifactBytes::empty(),
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
            artifacts: horsie_agentcore::ArtifactBytes::empty(),
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
            artifacts: horsie_agentcore::ArtifactBytes::empty(),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod artifact_tests {
    //! Artifacts on the Chat Completions wire, including the one thing this
    //! protocol cannot do: carry an image inside a tool result.
    use super::*;
    use horsie_agentcore::{ArtifactBytes, CompletionRequest, ToolChoice};
    use horsie_models::agent::{
        ArtifactKind, ArtifactPart, ArtifactRef, DocumentArtifact, ImageArtifact, Message,
        ToolResultPart,
    };
    use std::collections::HashMap;

    const DATA: &str = "iVBORw0KGgo=";

    fn image_ref() -> ArtifactRef {
        ArtifactRef {
            id: "abc123".into(),
            media_type: "image/png".into(),
            kind: ArtifactKind::Image(ImageArtifact {
                width: None,
                height: None,
            }),
            byte_size: 8,
            filename: None,
        }
    }

    fn provider() -> OpenAiProvider {
        OpenAiProvider::with_api_key("test-key")
            .expect("provider")
            .with_model("gpt-test")
    }

    fn body(messages: &[Message], artifacts: &ArtifactBytes) -> ChatCompletionRequest {
        provider().build_body(&CompletionRequest {
            messages,
            artifacts,
            system: None,
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_tokens: None,
            thinking_effort: None,
            conversation_id: "c1",
        })
    }

    fn user(parts: Vec<ContentPart>) -> Message {
        Message {
            id: "m1".into(),
            role: Role::User,
            parts,
            created_at_ms: 0,
            started_at_ms: None,
        }
    }

    fn hydrated() -> ArtifactBytes {
        ArtifactBytes::new(HashMap::from([("abc123".to_string(), DATA.to_string())]))
    }

    #[test]
    fn an_image_becomes_a_data_url_content_part() {
        let msgs = vec![user(vec![
            ContentPart::Text(TextPart {
                text: "what is this?".into(),
            }),
            ContentPart::Artifact(ArtifactPart {
                artifact: image_ref(),
            }),
        ])];
        let req = body(&msgs, &hydrated());
        let parts = match req.messages[0].content.as_ref().expect("content") {
            ChatContent::Parts(p) => p,
            ChatContent::Text(t) => panic!("expected parts, got text: {t}"),
        };
        assert_eq!(parts.len(), 2, "the text and the image");
        assert!(matches!(
            &parts[1],
            ChatContentPart::ImageUrl { image_url }
                if image_url.url == format!("data:image/png;base64,{DATA}")
        ));
    }

    /// A message with no artifact must stay a bare string, so every request
    /// this provider already sent is byte-identical.
    #[test]
    fn a_text_only_message_stays_a_bare_string() {
        let msgs = vec![user(vec![ContentPart::Text(TextPart {
            text: "hello".into(),
        })])];
        let req = body(&msgs, ArtifactBytes::empty());
        assert!(matches!(
            req.messages[0].content.as_ref().expect("content"),
            ChatContent::Text(t) if t == "hello"
        ));
    }

    /// The protocol asymmetry that shapes this provider: Chat Completions
    /// rejects an image inside a `tool` message, so a tool's screenshot has to
    /// follow as a user turn. Anthropic needs no such thing.
    #[test]
    fn a_tool_results_image_follows_as_a_user_message() {
        let msgs = vec![Message {
            id: "m1".into(),
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "tc1".into(),
                output: "took a screenshot".into(),
                is_error: false,
                artifacts: vec![image_ref()],
            })],
            created_at_ms: 0,
            started_at_ms: None,
        }];
        let req = body(&msgs, &hydrated());

        assert_eq!(req.messages.len(), 2, "the tool result, then the image");
        assert_eq!(req.messages[0].role, "tool");
        assert_eq!(
            req.messages[0].tool_call_id.as_deref(),
            Some("tc1"),
            "the tool message still answers its call"
        );
        assert_eq!(
            req.messages[1].role, "user",
            "an image can only live in a user message here"
        );
        let parts = match req.messages[1].content.as_ref().expect("content") {
            ChatContent::Parts(p) => p,
            ChatContent::Text(t) => panic!("expected parts, got text: {t}"),
        };
        assert!(matches!(&parts[1], ChatContentPart::ImageUrl { .. }));
    }

    #[test]
    fn a_pdf_becomes_a_file_part_with_a_filename() {
        let mut pdf = image_ref();
        pdf.media_type = "application/pdf".into();
        pdf.kind = ArtifactKind::Document(DocumentArtifact {});
        pdf.filename = Some("report.pdf".into());
        let msgs = vec![user(vec![ContentPart::Artifact(ArtifactPart {
            artifact: pdf,
        })])];
        let req = body(&msgs, &hydrated());
        let parts = match req.messages[0].content.as_ref().expect("content") {
            ChatContent::Parts(p) => p,
            ChatContent::Text(t) => panic!("expected parts, got text: {t}"),
        };
        assert!(matches!(
            &parts[0],
            ChatContentPart::File { file }
                if file.filename.as_deref() == Some("report.pdf")
        ));
    }

    #[test]
    fn an_unhydrated_artifact_becomes_text() {
        let msgs = vec![user(vec![ContentPart::Artifact(ArtifactPart {
            artifact: image_ref(),
        })])];
        let req = body(&msgs, ArtifactBytes::empty());
        assert!(matches!(
            req.messages[0].content.as_ref().expect("content"),
            ChatContent::Text(t) if t.contains("omitted")
        ));
    }
}
