//! One provider call: the whole of what this crate now does.
//!
//! The tool loop that used to live here is the agent actor's business — it
//! decides what each response means, executes tools, and asks for the next
//! call. This module owns exactly one step of it: assemble the request from
//! the history it was handed, stream the response, and hand back the
//! fully-formed assistant message with its cost.

use crate::{
    error::LlmError,
    events::EventSink,
    provider::{
        ArtifactBytes, ArtifactSource, CompletionRequest, LlmProvider, StopReason, ToolChoice,
    },
    tool::ToolSpec,
};
use horsie_models::agent::{ContentPart, Message, Role, Usage};
use horsie_models::events::{AgentEvent, MessageStartEvent, MessageStopEvent};
use horsie_models::now_ms;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Everything one provider call needs beyond the history itself.
///
/// Built by the caller once per turn and reused across the turn's steps; the
/// history is a separate argument because it is the one thing that changes
/// between them.
pub struct StepRequest {
    pub provider: Arc<dyn LlmProvider>,
    /// Which conversation this step belongs to — the same value for every
    /// step of one conversation. See [`CompletionRequest::conversation_id`].
    pub conversation_id: String,
    /// Empty sends no system prompt.
    pub system_prompt: String,
    pub specs: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u32>,
    pub thinking_effort: Option<crate::thinking::ThinkingEffort>,
    /// Where artifact bytes come from. `None` shows the model none.
    pub artifact_source: Option<Arc<dyn ArtifactSource>>,
}

/// What one step produced: the assembled assistant message and its cost.
#[derive(Debug, Clone)]
pub struct StepResponse {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Why a step produced nothing.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    #[error("cancelled")]
    Cancelled,
    #[error("provider error: {0}")]
    Provider(#[from] LlmError),
}

/// Make one provider call over `history`, streaming as it goes.
///
/// Hydrates artifact bytes fresh for the call — the history is the caller's
/// state, so a screenshot a tool produced earlier in the turn is already in it
/// and this is the call that must show it. Cancellation aborts the in-flight
/// completion: dropping the provider future tears down the underlying HTTP
/// stream, so a stopped turn stops burning tokens now.
///
/// # Errors
/// [`StepError::Cancelled`] when `cancel` fired first; the provider's own
/// failure otherwise. Nothing is assembled for either — only a completed call
/// yields a message.
pub async fn run_step(
    req: &StepRequest,
    history: &[Message],
    events: &dyn EventSink,
    cancel: &CancellationToken,
) -> Result<StepResponse, StepError> {
    if cancel.is_cancelled() {
        return Err(StepError::Cancelled);
    }
    let artifacts = match &req.artifact_source {
        None => ArtifactBytes::default(),
        Some(source) => {
            let wanted = artifact_ids(history);
            match wanted.is_empty() {
                true => ArtifactBytes::default(),
                false => ArtifactBytes::new(source.resolve(&wanted).await),
            }
        }
    };

    let request = CompletionRequest {
        messages: history,
        artifacts: &artifacts,
        system: (!req.system_prompt.is_empty()).then(|| req.system_prompt.clone()),
        tools: req.specs.clone(),
        tool_choice: req.tool_choice.clone(),
        max_tokens: req.max_tokens,
        thinking_effort: req.thinking_effort,
        conversation_id: &req.conversation_id,
    };

    let msg_id = Uuid::new_v4().to_string();
    let _ = events
        .emit(AgentEvent::MessageStart(MessageStartEvent {
            message_id: msg_id.clone(),
            role: Role::Assistant,
        }))
        .await;

    // Stamped here rather than at `MessageStart` so the figure is the provider
    // call's own span: everything between this and the assistant message's
    // `created_at_ms` is generation.
    let call_started_ms = now_ms();
    let response = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(StepError::Cancelled),
        result = req.provider.complete(request, &msg_id, events) => {
            result.map_err(StepError::Provider)?
        }
    };

    let _ = events
        .emit(AgentEvent::MessageStop(MessageStopEvent {
            message_id: msg_id.clone(),
        }))
        .await;

    Ok(StepResponse {
        message: Message {
            id: msg_id,
            role: Role::Assistant,
            parts: response.parts,
            created_at_ms: now_ms(),
            started_at_ms: Some(call_started_ms),
        },
        stop_reason: response.stop_reason,
        usage: response.usage,
    })
}

/// Every artifact id `messages` reference, in first-seen order.
///
/// Both places one can appear: a part of its own (what a person attached) and
/// the artifacts hanging off a tool result (what a tool produced).
#[must_use]
pub fn artifact_ids(messages: &[Message]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for message in messages {
        for part in &message.parts {
            let from_part: &[String] = &match part {
                ContentPart::Artifact(a) => vec![a.artifact.id.clone()],
                ContentPart::ToolResult(r) => r.artifacts.iter().map(|a| a.id.clone()).collect(),
                ContentPart::Text(_)
                | ContentPart::ToolCall(_)
                | ContentPart::Thinking(_)
                | ContentPart::SubAgentResult(_) => Vec::new(),
            };
            for id in from_part {
                if seen.insert(id.clone()) {
                    ids.push(id.clone());
                }
            }
        }
    }
    ids
}

/// The tool calls in a response's parts, in request order.
#[must_use]
pub fn extract_tool_calls(parts: &[ContentPart]) -> Vec<(String, String, Value)> {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::ToolCall(tc) => Some((tc.id.clone(), tc.name.clone(), tc.input.clone())),
            ContentPart::Text(_)
            | ContentPart::ToolResult(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_)
            | ContentPart::Artifact(_) => None,
        })
        .collect()
}

/// The concatenated text of a response's parts.
#[must_use]
pub fn extract_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.text.as_str()),
            ContentPart::ToolCall(_)
            | ContentPart::ToolResult(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_)
            | ContentPart::Artifact(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// One string per identical batch of tool calls, for stuck detection.
#[must_use]
pub fn tool_fingerprint(tool_calls: &[(String, String, Value)]) -> String {
    tool_calls
        .iter()
        .map(|(_, name, input)| format!("{name}:{input}"))
        .collect::<Vec<_>>()
        .join("|")
}
