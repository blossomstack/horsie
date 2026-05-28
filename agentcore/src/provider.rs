use crate::{error::LlmError, events::EventSink, tool::ToolSpec};
use async_trait::async_trait;
use models::agent::{ContentPart, Message, Usage};

pub struct CompletionRequest<'a> {
    pub messages: &'a [Message],
    pub system: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u32>,
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
