//! The bare summarising step, and the budget that decides when it runs.
//!
//! Shared because two components spend it: compaction folds the summary back
//! into this agent's own history, and seeding hands it to sub sessions
//! branching off. Neither may name the other, so the machinery lives here.

use crate::agent_loop::prelude::*;
use horsie_agentcore::{ContentPart, Message, Role};
use horsie_models::now_ms;

/// The share of a model's context window at which an agent compacts.
///
/// A server constant rather than a session setting: the right value is a
/// property of the model, not of the session, so it stays retunable centrally
/// instead of frozen into everyone's saved presets. The headroom above it is
/// also what absorbs this check's one-iteration lag — `context_tokens` is the
/// last provider call's prompt size and does not count tool results appended
/// since.
pub(crate) const COMPACT_AT_PERCENT: u32 = 80;

/// Roughly how much of the window a compaction leaves as raw recent messages.
///
/// Not zero, because a summary alone loses the file path or error the agent was
/// part-way through, and those live in the last few messages.
pub(crate) const COMPACT_RETAIN_PERCENT: u32 = 20;

/// Swallows everything a summarise step streams. A summary is not a turn,
/// and its deltas must never reach a transcript — a viewer would watch the
/// summary being typed as though the agent had started answering.
struct NullSink;

#[async_trait::async_trait]
impl horsie_agentcore::EventSink for NullSink {
    async fn emit(
        &self,
        _event: horsie_agentcore::AgentEvent,
    ) -> Result<(), horsie_agentcore::EventSinkError> {
        Ok(())
    }
}

/// The shared summarise utility: the same [`horsie_agentcore::run_step`] the
/// turn drives, configured bare — no tools, no system prompt (workspace and
/// tool guidance are instructions for doing the work, and this step is not
/// doing the work), no artifacts (re-uploading every image to shrink the
/// context defeats the point), nothing streamed. Used here and by the seeding
/// component, which is what "sharing the compaction machinery" means.
///
/// Answers the summary and what the step spent. An empty span summarises to
/// nothing rather than erroring — a sub session branched from a session that
/// has not started yet is empty, not broken.
pub(crate) async fn summarise_step(
    execution: &ExecutionContext,
    history: &[Message],
    cut: usize,
    instructions: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(String, horsie_agentcore::Usage), horsie_agentcore::StepError> {
    let cut = cut.min(history.len());
    if cut == 0 {
        return Ok((String::new(), horsie_agentcore::Usage::without_cache(0, 0)));
    }
    let mut window = history[..cut].to_vec();
    window.push(Message {
        id: format!("compaction-request:{cut}"),
        role: Role::User,
        parts: vec![ContentPart::Text(horsie_models::agent::TextPart {
            text: horsie_agentcore::summary_prompt(instructions),
        })],
        created_at_ms: now_ms(),
        started_at_ms: None,
    });
    let request = horsie_agentcore::StepRequest {
        provider: execution.provider.clone(),
        conversation_id: execution.conversation_id.clone(),
        system_prompt: String::new(),
        specs: Vec::new(),
        tool_choice: horsie_agentcore::ToolChoice::Auto,
        max_tokens: None,
        thinking_effort: None,
        artifact_source: None,
    };
    let response = horsie_agentcore::run_step(&request, &window, &NullSink, cancel).await?;
    let text = horsie_agentcore::extract_text(&response.message.parts);
    if text.trim().is_empty() {
        return Err(horsie_agentcore::StepError::Provider(
            horsie_agentcore::LlmError::ApiError {
                status: 502,
                message: "the summariser returned no text".into(),
            },
        ));
    }
    Ok((text, response.usage))
}
