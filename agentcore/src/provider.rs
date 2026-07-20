use crate::{error::LlmError, events::EventSink, tool::ToolSpec};
use async_trait::async_trait;
use horsie_models::agent::{ContentPart, Message, Usage};

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

/// What a backend can do, so the agent loop can degrade rather than send a
/// request the backend will reject.
///
/// Only fields with a live consumer belong here. Anthropic-specific behavior
/// that is already encapsulated in its own provider crate (prompt caching,
/// thinking replay, error-body classification) needs no flag — the trait
/// boundary is doing that job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Whether the backend honors `tool_choice`. OpenAI proper does; Ollama and
    /// llama.cpp may ignore or reject it. When false, a forced handoff degrades
    /// to `Auto` and relies on the loop's nudge-and-retry.
    pub supports_tool_choice: bool,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            supports_tool_choice: true,
        }
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model_id(&self) -> &str;

    /// What this backend can do. Defaults to fully capable — an existing
    /// provider needs no change.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Perform a completion. `message_id` is the agent-assigned ID for the assistant
    /// message being generated; providers should tag any streaming events they emit with it.
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError>;
}
