//! Deciding what a compaction keeps, and the seam it reaches the owner through.
//!
//! Two things live here. [`choose_cut`] is a pure function over a slice of
//! messages — the interesting part is a table of cases, and a table of cases
//! wants isolated tests, the same reason `agent_log.rs` is its own module in
//! the server. [`CompactionPolicy`] is the trait through which everything the
//! agent cannot know — the exact state to carry, the hooks to fire — is
//! supplied by whoever owns it.

use horsie_models::agent::{ContentPart, Message, Role};

/// How much room a compaction is working with.
///
/// Absent from an [`crate::AgentConfig`] means this agent never compacts on its
/// own: a workflow step, a test fixture, or a model whose card declares no
/// context window. Guessing a window would either compact a session that had
/// room or fail to compact one that did not, and both are worse than leaving it
/// to `/compact`.
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

/// What a compaction is about to do, for a hook that may refuse it.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// How many messages will be folded into the summary.
    pub covered: usize,
    /// How many will survive verbatim.
    pub retained: usize,
    pub tokens_before: u32,
    pub instructions: Option<String>,
}

/// A `PreCompact` hook's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCompactDecision {
    Proceed,
    /// A hook blocked or halted. The compaction is abandoned and the turn
    /// continues uncompacted — which may then overflow, honestly, rather than
    /// silently proceeding without the state a hook was about to save.
    Abandon(String),
}

/// What a compaction achieved, for `PostCompact`.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub summary: String,
    pub tokens_before: u32,
    pub tokens_after: u32,
}

/// The owner's half of a compaction.
///
/// The agent knows when the budget is crossed, holds the provider that can
/// summarise, and owns the history to rewrite. It does not know what else is
/// true of the session — which tasks are open, which timers are armed, what a
/// plugin wants to do about any of it. That is all here.
///
/// An agent built without one never compacts.
#[async_trait::async_trait]
pub trait CompactionPolicy: Send + Sync {
    /// Facts too exact to paraphrase, rendered by whoever owns them.
    ///
    /// Read at the boundary rather than at run start: the model can add a task
    /// or arm a timer part-way through a turn, and a mid-loop compaction has to
    /// see it.
    async fn carried_state(&self) -> String;

    /// Fired before any history is rewritten.
    async fn before(&self, plan: &CompactionPlan) -> PreCompactDecision;

    /// Fired once the boundary exists.
    async fn after(&self, result: &CompactionResult);
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
        })
        .sum();
    u32::try_from(chars / 4).unwrap_or(u32::MAX)
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::agent::{TextPart, ToolCallPart, ToolResultPart};

    fn msg(role: Role, id: &str, text: &str) -> Message {
        Message {
            id: id.into(),
            role,
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
            created_at_ms: 0,
            started_at_ms: None,
        }
    }

    fn user(id: &str, text: &str) -> Message {
        msg(Role::User, id, text)
    }

    fn assistant_calling(id: &str, tool_call_id: &str) -> Message {
        Message {
            id: id.into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: tool_call_id.into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            })],
            created_at_ms: 0,
            started_at_ms: None,
        }
    }

    fn tool_result(tool_call_id: &str, output: &str) -> Message {
        Message {
            id: format!("result:{tool_call_id}"),
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: tool_call_id.into(),
                output: output.into(),
                is_error: false,
            })],
            created_at_ms: 0,
            started_at_ms: None,
        }
    }

    #[test]
    fn an_empty_history_cuts_at_zero() {
        assert_eq!(choose_cut(&[], 1_000), 0);
    }

    #[test]
    fn the_cut_lands_on_a_user_message_boundary() {
        let history = vec![
            user("u0", &"a".repeat(4_000)),
            msg(Role::Assistant, "a0", "ok"),
            user("u1", "second question"),
            msg(Role::Assistant, "a1", "answer"),
        ];
        // `u0` is enormous and `u1` is not. The budget reaches back past the
        // assistant message at index 1, and the trap is to keep walking from
        // there to the next safe boundary — which is index 0, dragging the
        // 1000-token message the budget existed to exclude back in. The answer
        // is the *earliest safe boundary that still fits*, not the first safe
        // thing found while walking back.
        assert_eq!(
            choose_cut(&history, 100),
            2,
            "the window must start on a user message, and on the earliest one \
             that fits rather than the first one a walk-back stumbles on"
        );
    }

    /// The invariant the whole cut exists for. A window opening on a tool
    /// result hands a provider an answer to a call it cannot see, which is the
    /// dangling-`tool_use_id` failure this codebase has hit before.
    #[test]
    fn the_cut_never_separates_an_assistant_message_from_its_tool_results() {
        let history = vec![
            user("u0", "do the thing"),
            assistant_calling("a0", "tc1"),
            tool_result("tc1", &"x".repeat(8_000)),
            msg(Role::Assistant, "a1", "done"),
        ];
        // A tight budget would prefer to start at the tool result or later.
        let cut = choose_cut(&history, 10);
        assert_eq!(
            cut, 0,
            "no safe boundary exists after the user message, so the window \
             opens there rather than mid-turn"
        );
        assert!(is_safe_boundary(&history[cut]));
    }

    #[test]
    fn one_turn_larger_than_the_budget_retains_nothing() {
        // No user message at all: nothing here is safe to open a window on.
        let history = vec![
            assistant_calling("a0", "tc1"),
            tool_result("tc1", &"x".repeat(40_000)),
        ];
        assert_eq!(
            choose_cut(&history, 10),
            history.len(),
            "a compaction with no coherent partial view is summary-only"
        );
    }

    #[test]
    fn a_history_that_fits_entirely_is_retained_whole() {
        let history = vec![user("u0", "hi"), msg(Role::Assistant, "a0", "hello")];
        assert_eq!(choose_cut(&history, 10_000), 0);
    }

    #[test]
    fn instructions_are_appended_to_the_summary_prompt_not_substituted() {
        let plain = summary_prompt(None);
        let focused = summary_prompt(Some("keep the migration details"));
        assert!(focused.starts_with(&plain), "the standard prompt survives");
        assert!(focused.contains("keep the migration details"));
        assert_eq!(
            summary_prompt(Some("   ")),
            plain,
            "blank instructions are no instructions"
        );
    }
}
