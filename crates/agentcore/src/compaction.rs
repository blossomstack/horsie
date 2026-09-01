//! Deciding what a compaction keeps, and the summarising call itself.
//!
//! Everything here is either a pure function over a slice of messages
//! ([`choose_cut`], [`summary_prompt`], [`boundary_text`]) or one provider
//! call ([`summarise_span`]). Orchestration — when to compact, which hooks to
//! fire, what state to carry across — belongs to the agent actor, which owns
//! the history and the journal.

use crate::error::LlmError;
use crate::events::{EventSink, EventSinkError};
use crate::provider::{CompletionRequest, LlmProvider, ToolChoice};
use horsie_models::agent::{ContentPart, Message, Role, TextPart};
use horsie_models::events::AgentEvent;
use horsie_models::now_ms;
use std::sync::Arc;

/// How much room a compaction is working with.
///
/// `None` in the caller means the agent never compacts on its own: a workflow
/// step, a test fixture, or a model whose card declares no context window.
/// Guessing a window would either compact a session that had room or fail to
/// compact one that did not, and both are worse than leaving it to `/compact`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionBudget {
    pub context_window: u32,
    /// Compact once the last prompt reached this share of the window.
    pub trigger_at_percent: u32,
    /// Roughly how much of the window to leave as raw recent messages.
    pub retain_percent: u32,
}

impl CompactionBudget {
    /// The prompt size at which this agent should compact.
    #[must_use]
    pub fn trigger_tokens(&self) -> u32 {
        self.context_window
            .saturating_mul(self.trigger_at_percent)
            .saturating_div(100)
    }

    /// Roughly how many tokens of raw recent history to keep.
    #[must_use]
    pub fn retain_tokens(&self) -> u32 {
        self.context_window
            .saturating_mul(self.retain_percent)
            .saturating_div(100)
    }
}

/// An artifact's size in the same char-per-4-tokens unit [`approx_tokens`]
/// works in.
///
/// Anthropic prices an image at roughly `width * height / 750` tokens, which is
/// the only published figure precise enough to be worth using; the flat
/// fallbacks are order-of-magnitude guesses for the cases where there is
/// nothing to compute from. As with the rest of this function, being well out
/// moves the cut by a message and never decides whether to compact.
fn artifact_chars(artifact: &horsie_models::agent::ArtifactRef) -> usize {
    /// An image whose header would not parse.
    const UNKNOWN_IMAGE_TOKENS: usize = 1_500;
    /// A PDF, which the provider rasterises per page.
    const DOCUMENT_TOKENS: usize = 3_000;

    let tokens = match &artifact.kind {
        horsie_models::agent::ArtifactKind::Image(image) => match (image.width, image.height) {
            (Some(w), Some(h)) => (w as usize * h as usize) / 750,
            _ => UNKNOWN_IMAGE_TOKENS,
        },
        horsie_models::agent::ArtifactKind::Document(_) => DOCUMENT_TOKENS,
    };
    tokens * 4
}

/// A rough token count, for choosing how much history to keep.
///
/// Four characters to the token, which is wrong in the third significant digit
/// on every wire and does not matter: this decides how much of the *retain*
/// budget has been used, and being 15% out moves the cut by one message. The
/// decision that must not drift — whether to compact at all — never comes
/// through here. It reads the provider's own reported prompt size.
fn approx_tokens(message: &Message) -> u32 {
    let chars: usize = message
        .parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(t) => t.text.len(),
            ContentPart::ToolResult(r) => r.output.len(),
            ContentPart::ToolCall(c) => c.input.to_string().len(),
            ContentPart::Thinking(t) => t.text.len(),
            ContentPart::SubAgentResult(r) => r.text.len(),
            // An artifact contributes no characters but is a long way from
            // free, so counting it as zero would let the retained window hold
            // far more than the budget it was measured against.
            ContentPart::Artifact(a) => artifact_chars(&a.artifact),
        })
        .sum();
    u32::try_from(chars / 4).unwrap_or(u32::MAX)
}

/// A rough token count for a whole history, in [`approx_tokens`] units. What a
/// caller stamps on a boundary as `tokens_after`.
#[must_use]
pub fn approx_history_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .map(approx_tokens)
        .fold(0u32, u32::saturating_add)
}

/// Whether a message may begin the retained window.
///
/// A real user message and nothing else. A tool result is `Role::Tool`, so
/// starting there would hand a provider an answer to a call it cannot see; an
/// assistant message may carry the `tool_use` whose results follow it.
fn is_safe_boundary(message: &Message) -> bool {
    message.role == Role::User
        && !message
            .parts
            .iter()
            .any(|p| matches!(p, ContentPart::ToolResult(_)))
}

/// The index the retained window starts at.
///
/// The **earliest safe boundary whose suffix still fits the budget** — which is
/// the most history that can be kept without either overrunning the budget or
/// opening the window mid-turn.
///
/// Two fallbacks, in order. When no safe boundary fits, the *latest* one is
/// used: overrunning the budget is a cost, and a prompt whose first message
/// answers a tool call it cannot see is broken, so the budget is what gives.
/// When there is no safe boundary at all, `history.len()` is returned and the
/// compaction is summary-only — a single turn larger than the budget has no
/// partial view that is coherent.
///
/// Written as "scan for the earliest that fits" rather than "walk back from the
/// tail until the budget runs out, then keep walking to something safe": the
/// second is the obvious phrasing and it is wrong, because the walk-back can
/// step *past* a safe boundary that was inside the budget and land on one far
/// earlier, retaining an enormous message the budget was meant to exclude.
#[must_use]
pub fn choose_cut(history: &[Message], retain_budget_tokens: u32) -> usize {
    if history.is_empty() {
        return 0;
    }
    // Tokens from each index to the end, so a candidate is tested in O(1).
    let mut suffix = vec![0u32; history.len() + 1];
    for idx in (0..history.len()).rev() {
        suffix[idx] = suffix[idx + 1].saturating_add(approx_tokens(&history[idx]));
    }

    let mut latest_safe = None;
    for (idx, message) in history.iter().enumerate() {
        if !is_safe_boundary(message) {
            continue;
        }
        if suffix[idx] <= retain_budget_tokens {
            return idx;
        }
        latest_safe = Some(idx);
    }
    latest_safe.unwrap_or(history.len())
}

/// Build the request that asks a model to summarise a span.
///
/// A fixed structure rather than a free "summarise this": the sections are what
/// a resumed agent turns out to need, and naming them is what stops a summary
/// becoming a paragraph of atmosphere. `instructions` is appended, not
/// substituted, so `/compact keep the migration details` adds a focus rather
/// than discarding the rest.
#[must_use]
pub fn summary_prompt(instructions: Option<&str>) -> String {
    let mut prompt = String::from(
        "Summarise the conversation so far for an agent that will continue the \
         work without being able to see it. Write for a reader who has the \
         recent messages but none of the earlier ones.\n\n\
         Cover, as headed sections, and omit a section only when it is truly \
         empty:\n\
         - What was asked, and what the user actually wants\n\
         - Decisions taken, and the reasoning that is not obvious from the result\n\
         - Files and code touched, by exact path\n\
         - Errors hit and how they were resolved\n\
         - What is in flight right now\n\
         - The immediate next step\n\n\
         Be specific. Names, paths, ids and error text are worth more than \
         prose. Do not invent anything that is not in the conversation, and do \
         not address the user.",
    );
    if let Some(extra) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str("\n\nThe user asked you to pay particular attention to: ");
        prompt.push_str(extra);
    }
    prompt
}

/// Swallows everything. The summarising call is not a turn, and its streaming
/// deltas must never reach a transcript — a viewer would watch the summary
/// being typed as though the agent had started answering.
struct NullSink;

#[async_trait::async_trait]
impl EventSink for NullSink {
    async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
        Ok(())
    }
}

/// Ask the model to summarise `history[..cut]`.
///
/// `cut == history.len()` summarises everything, which is what a sub session's
/// seed and a summary-only compaction both want. An empty span summarises to
/// nothing rather than erroring — a sub session branched from a session that
/// has not started yet is empty, not broken.
///
/// # Errors
/// Whatever the summarising provider call fails with, plus a summariser that
/// answered with no text at all.
pub async fn summarise_span(
    provider: &Arc<dyn LlmProvider>,
    conversation_id: &str,
    history: &[Message],
    cut: usize,
    instructions: Option<&str>,
    max_tokens: Option<u32>,
) -> Result<String, LlmError> {
    let cut = cut.min(history.len());
    if cut == 0 {
        return Ok(String::new());
    }
    let mut messages = history[..cut].to_vec();
    messages.push(Message {
        id: format!("compaction-request:{cut}"),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: summary_prompt(instructions),
        })],
        created_at_ms: now_ms(),
        started_at_ms: None,
    });
    let response = provider
        .complete(
            CompletionRequest {
                messages: &messages,
                // A summariser reads the transcript's text. Handing it the
                // images too would re-upload every one of them for a call
                // whose whole purpose is to make the context smaller.
                artifacts: crate::provider::ArtifactBytes::empty(),
                // No system prompt: the workspace and tool guidance are
                // instructions for doing the work, and this call is not
                // doing the work. They would only bias the summary.
                system: None,
                // No tools, which is also what makes `tool_choice`
                // irrelevant — every provider omits it when tools are
                // empty.
                tools: Vec::new(),
                tool_choice: ToolChoice::Auto,
                max_tokens,
                thinking_effort: None,
                conversation_id,
            },
            "compaction",
            &NullSink,
        )
        .await?;

    let text = crate::step::extract_text(&response.parts);
    if text.trim().is_empty() {
        return Err(LlmError::ApiError {
            status: 502,
            message: "the summariser returned no text".into(),
        });
    }
    Ok(text)
}

/// The exact text of a boundary message. One function so the run and the
/// recovered log cannot drift apart.
#[must_use]
pub fn boundary_text(summary: &str, carried_state: &str) -> String {
    format!(
        "This conversation was compacted: earlier history is summarised below \
         rather than shown in full. The messages after this one are verbatim.\n\n\
         ## Summary of earlier work\n{}\n\n## Current state\n{}",
        summary.trim(),
        carried_state.trim(),
    )
}
