use crate::{error::LlmError, events::EventSink, tool::ToolSpec};
use async_trait::async_trait;
use horsie_models::agent::{ContentPart, Message, Usage};

pub struct CompletionRequest<'a> {
    pub messages: &'a [Message],
    pub system: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u32>,
    /// Canonical thinking effort for this session; `None` sends no control.
    pub thinking_effort: Option<crate::thinking::ThinkingEffort>,
    /// Which conversation this turn belongs to: the same value for every turn of
    /// one conversation, and different across conversations.
    ///
    /// Required rather than optional so it cannot be quietly omitted. A provider
    /// that groups requests by it (the Responses wire sends it as
    /// `prompt_cache_key`) gets no second chance if it is missing — the effect
    /// of a wrong value is a silently colder cache, never an error. Providers
    /// with no use for it ignore it.
    pub conversation_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub parts: Vec<ContentPart>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone)]
pub enum ToolChoice {
    Auto,
    Any,
    Required(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model_id(&self) -> &str;

    /// Perform a completion. `message_id` is the agent-assigned ID for the assistant
    /// message being generated; providers should tag any streaming events they emit with it.
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError>;
}
