use agentcore::{
    AgentEvent, CompletionRequest, CompletionResponse, ContentPart, EventSink, LlmError,
    LlmProvider, StopReason, TextChunkEvent, TextPart, ThinkingChunkEvent, ThinkingPart,
    ToolCallInputDeltaEvent, ToolCallInputDoneEvent, ToolCallPart, ToolCallStartEvent, ToolChoice,
    Usage,
};
use async_anthropic::{
    Client,
    types::{
        CacheControl, ContentBlockDelta, CreateMessagesRequestBuilder, MessageBuilder,
        MessageContent, MessageRole, MessagesStreamEvent, Text, Thinking, ThinkingConfig,
        ToolResult, ToolUse,
    },
};
use async_trait::async_trait;
use std::{collections::HashMap, env, time::Duration};
use tokio_stream::StreamExt;

pub const DEFAULT_MODEL: &str = "claude-3-5-sonnet-20241022";
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;
const MAX_STREAM_RETRIES: u32 = 6;
const BACKOFF_BASE_SECS: u64 = 5;

pub fn env_base_url() -> Option<String> {
    env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

fn is_retryable(msg: &str) -> bool {
    msg.contains("overloaded_error")
        || msg.contains("overloaded")
        || msg.contains("rate_limit_error")
        || msg.contains("529")
        || msg.contains("Too Many Requests")
}

fn to_llm_error(e: async_anthropic::errors::AnthropicError) -> LlmError {
    let msg = e.to_string();
    if msg.contains("overloaded_error") || msg.contains("overloaded") || msg.contains("529") {
        return LlmError::Overloaded;
    }
    if msg.contains("rate_limit_error") || msg.contains("Too Many Requests") || msg.contains("429")
    {
        return LlmError::RateLimit { retry_after: None };
    }
    use async_anthropic::errors::AnthropicError;
    match e {
        AnthropicError::NetworkError(re) => LlmError::Network(Box::new(re)),
        AnthropicError::Unauthorized => LlmError::ApiError {
            status: 401,
            message: "Unauthorized".into(),
        },
        AnthropicError::BadRequest(m)
        | AnthropicError::ApiError(m)
        | AnthropicError::Unknown(m) => LlmError::Network(Box::new(std::io::Error::other(m))),
        AnthropicError::DeserializationError(de) => {
            LlmError::Network(Box::new(std::io::Error::other(de.to_string())))
        }
        AnthropicError::UnexpectedError => {
            LlmError::Network(Box::new(std::io::Error::other("unexpected error")))
        }
        AnthropicError::StreamError(se) => {
            LlmError::Network(Box::new(std::io::Error::other(se.to_string())))
        }
    }
}

fn io_err(msg: impl std::fmt::Display) -> LlmError {
    LlmError::Network(Box::new(std::io::Error::other(msg.to_string())))
}

pub struct AnthropicProvider {
    client: Client,
    model: String,
    api_key: Option<String>,
    base_url: Option<String>,
    session_id: Option<String>,
    thinking_budget: Option<u32>,
    max_tokens: Option<u32>,
    retry_base_secs: u64,
}

impl AnthropicProvider {
    fn build_client(
        api_key: Option<&str>,
        base_url: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Client, LlmError> {
        let mut http = reqwest::Client::builder();
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
        let client = Self::build_client(None, base_url.as_deref(), None)?;
        Ok(Self {
            client,
            model: DEFAULT_MODEL.into(),
            api_key: None,
            base_url,
            session_id: None,
            thinking_budget: None,
            max_tokens: None,
            retry_base_secs: BACKOFF_BASE_SECS,
        })
    }

    pub fn with_api_key(key: impl Into<String>) -> Result<Self, LlmError> {
        let key = key.into();
        let base_url = env_base_url();
        let client = Self::build_client(Some(&key), base_url.as_deref(), None)?;
        Ok(Self {
            client,
            model: DEFAULT_MODEL.into(),
            api_key: Some(key),
            base_url,
            session_id: None,
            thinking_budget: None,
            max_tokens: None,
            retry_base_secs: BACKOFF_BASE_SECS,
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

    #[must_use]
    pub fn with_retry_delay_secs(mut self, secs: u64) -> Self {
        self.retry_base_secs = secs;
        self
    }

    fn rebuild_client(&mut self) {
        match Self::build_client(
            self.api_key.as_deref(),
            self.base_url.as_deref(),
            self.session_id.as_deref(),
        ) {
            Ok(c) => self.client = c,
            Err(e) => tracing::warn!("failed to rebuild Anthropic client: {e}"),
        }
    }

    fn to_api_role(role: &models::agent::Role) -> MessageRole {
        match role {
            models::agent::Role::Assistant => MessageRole::Assistant,
            models::agent::Role::User | models::agent::Role::Tool => MessageRole::User,
        }
    }

    fn parts_to_api_content(parts: &[ContentPart]) -> async_anthropic::types::MessageContentList {
        use async_anthropic::types::MessageContentList;
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
                    signature: th.signature.clone().unwrap_or_default(),
                    ..Default::default()
                }),
            })
            .collect();
        MessageContentList(items)
    }

    fn mark_last_message_cacheable(messages: &mut [async_anthropic::types::Message]) {
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
        let mut api_messages: Vec<async_anthropic::types::Message> = request
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
                    builder.tool_choice(async_anthropic::types::ToolChoice::Any);
                }
                ToolChoice::Required(name) => {
                    builder.tool_choice(async_anthropic::types::ToolChoice::Tool(name.clone()));
                }
            }
        }
        if let Some(budget) = self.thinking_budget {
            builder.thinking(ThinkingConfig::Enabled {
                budget_tokens: budget,
            });
        }
        let api_request = builder.build().map_err(io_err)?;

        // 4. Stream with retry (only when no content has been emitted yet)
        let mut text_blocks: HashMap<usize, String> = HashMap::new();
        let mut tool_blocks: HashMap<usize, (String, String, String)> = HashMap::new();
        let mut thinking_blocks: HashMap<usize, (String, String)> = HashMap::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut last_error: Option<LlmError> = None;

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
                        let msg = e.to_string();
                        if is_retryable(&msg)
                            && text_blocks.is_empty()
                            && tool_blocks.is_empty()
                            && thinking_blocks.is_empty()
                        {
                            last_error = Some(to_llm_error(e));
                            continue 'retry;
                        }
                        return Err(to_llm_error(e));
                    }
                };

                match event {
                    MessagesStreamEvent::MessageStart { message, usage: _ } => {
                        if let Some(u) = &message.usage {
                            input_tokens = u.input_tokens.unwrap_or(0);
                        }
                    }
                    MessagesStreamEvent::ContentBlockStart {
                        index,
                        content_block,
                    } => match content_block {
                        MessageContent::Text(_) => {
                            text_blocks.insert(index, String::new());
                        }
                        MessageContent::ToolUse(tu) => {
                            events
                                .emit(AgentEvent::ToolCallStart(ToolCallStartEvent {
                                    message_id: message_id.to_string(),
                                    index: index as u32,
                                    tool_call_id: tu.id.clone(),
                                    name: tu.name.clone(),
                                }))
                                .await?;
                            tool_blocks.insert(index, (tu.id, tu.name, String::new()));
                        }
                        MessageContent::Thinking(_) => {
                            thinking_blocks.insert(index, (String::new(), String::new()));
                        }
                        MessageContent::ToolResult(_) => {}
                    },
                    MessagesStreamEvent::ContentBlockDelta { index, delta } => match delta {
                        ContentBlockDelta::TextDelta { text } => {
                            if let Some(acc) = text_blocks.get_mut(&index) {
                                acc.push_str(&text);
                            }
                            events
                                .emit(AgentEvent::TextChunk(TextChunkEvent {
                                    message_id: message_id.to_string(),
                                    index: index as u32,
                                    text,
                                }))
                                .await?;
                        }
                        ContentBlockDelta::InputJsonDelta { partial_json } => {
                            if let Some((id, _, acc)) = tool_blocks.get_mut(&index) {
                                acc.push_str(&partial_json);
                                events
                                    .emit(AgentEvent::ToolCallInputDelta(ToolCallInputDeltaEvent {
                                        message_id: message_id.to_string(),
                                        index: index as u32,
                                        tool_call_id: id.clone(),
                                        delta: partial_json,
                                    }))
                                    .await?;
                            }
                        }
                        ContentBlockDelta::ThinkingDelta { thinking } => {
                            if let Some((acc, _)) = thinking_blocks.get_mut(&index) {
                                acc.push_str(&thinking);
                            }
                            events
                                .emit(AgentEvent::ThinkingChunk(ThinkingChunkEvent {
                                    message_id: message_id.to_string(),
                                    index: index as u32,
                                    text: thinking,
                                }))
                                .await?;
                        }
                        ContentBlockDelta::SignatureDelta { signature } => {
                            if let Some((_, acc_sig)) = thinking_blocks.get_mut(&index) {
                                acc_sig.push_str(&signature);
                            }
                        }
                    },
                    MessagesStreamEvent::ContentBlockStop { index } => {
                        if let Some((id, _, _)) = tool_blocks.get(&index) {
                            events
                                .emit(AgentEvent::ToolCallInputDone(ToolCallInputDoneEvent {
                                    message_id: message_id.to_string(),
                                    index: index as u32,
                                    tool_call_id: id.clone(),
                                }))
                                .await?;
                        }
                    }
                    MessagesStreamEvent::MessageDelta { delta, usage } => {
                        stop_reason = match delta.stop_reason.as_deref() {
                            Some("tool_use") => StopReason::ToolUse,
                            Some("max_tokens") => StopReason::MaxTokens,
                            Some(_) | None => StopReason::EndTurn,
                        };
                        if let Some(u) = usage {
                            output_tokens = u.output_tokens.unwrap_or(output_tokens);
                        }
                    }
                    MessagesStreamEvent::MessageStop => {}
                }
            }

            break 'retry;
        }

        if text_blocks.is_empty()
            && tool_blocks.is_empty()
            && thinking_blocks.is_empty()
            && let Some(e) = last_error
        {
            return Err(e);
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
                let input = match serde_json::from_str::<serde_json::Value>(json_str) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(
                            tool = %name,
                            partial = %json_str.chars().take(80).collect::<String>(),
                            "failed to parse tool input JSON: {e}"
                        );
                        serde_json::Value::Object(serde_json::Map::default())
                    }
                };
                parts.push(ContentPart::ToolCall(ToolCallPart {
                    id: id.clone(),
                    name: name.clone(),
                    input,
                }));
            } else if let Some((thinking, signature)) = thinking_blocks.get(&idx)
                && !thinking.is_empty()
            {
                parts.push(ContentPart::Thinking(ThinkingPart {
                    text: thinking.clone(),
                    signature: if signature.is_empty() {
                        None
                    } else {
                        Some(signature.clone())
                    },
                }));
            }
        }

        Ok(CompletionResponse {
            parts,
            stop_reason,
            usage: Usage {
                input_tokens,
                output_tokens,
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
    use super::*;

    #[test]
    fn test_to_api_role_user() {
        assert!(matches!(
            AnthropicProvider::to_api_role(&models::agent::Role::User),
            MessageRole::User
        ));
    }

    #[test]
    fn test_to_api_role_assistant() {
        assert!(matches!(
            AnthropicProvider::to_api_role(&models::agent::Role::Assistant),
            MessageRole::Assistant
        ));
    }

    #[test]
    fn test_to_api_role_tool_maps_to_user() {
        assert!(matches!(
            AnthropicProvider::to_api_role(&models::agent::Role::Tool),
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

    #[test]
    fn test_parts_to_api_content_tool_result() {
        let parts = vec![ContentPart::ToolResult(models::agent::ToolResultPart {
            tool_call_id: "tc1".into(),
            output: "result".into(),
            is_error: false,
        })];
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
            matches!(&list[0], MessageContent::Thinking(t) if t.thinking == "think" && t.signature == "sig123")
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
}
