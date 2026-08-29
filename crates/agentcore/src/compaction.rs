//! Deciding what a compaction keeps, and the seam it reaches the owner through.
//!
//! Two things live here. [`choose_cut`] is a pure function over a slice of
//! messages — the interesting part is a table of cases, and a table of cases
//! wants isolated tests, the same reason `agent_log.rs` is its own module in
//! the server. [`CompactionPolicy`] is the trait through which everything the
//! agent cannot know — the exact state to carry, the hooks to fire — is
//! supplied by whoever owns it.

use crate::agent::Agent;
use crate::error::AgentError;
use crate::events::{EventSink, EventSinkError};
use crate::provider::{CompletionRequest, ToolChoice};
use horsie_models::agent::{
    CompactionSkippedLifecycle, CompactionTrigger, ContentPart, EmptyOutcome, Message, Role,
    TextPart,
};
use horsie_models::events::{AgentEvent, CompactedEvent, CompactionSkippedEvent};
use horsie_models::now_ms;

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
    /// `auto` or `manual`. Read off the trigger rather than from whether
    /// instructions were given: a bare `/compact` carries none and is still
    /// manual.
    pub trigger: &'static str,
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
    /// `auto` or `manual` — carried so a `PostCompact` hook matches on the same
    /// domain its `PreCompact` did.
    pub trigger: &'static str,
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
            // An artifact contributes no characters but is a long way from
            // free, so counting it as zero would let the retained window hold
            // far more than the budget it was measured against.
            ContentPart::Artifact(a) => artifact_chars(&a.artifact),
        })
        .sum();
    u32::try_from(chars / 4).unwrap_or(u32::MAX)
}

/// An artifact's size in the same char-per-4-tokens unit [`approx_tokens`] works
/// in.
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
        horsie_models::agent::ArtifactKind::Image(image) => {
            match (image.width, image.height) {
                (Some(w), Some(h)) => (w as usize * h as usize) / 750,
                _ => UNKNOWN_IMAGE_TOKENS,
            }
        }
        horsie_models::agent::ArtifactKind::Document(_) => DOCUMENT_TOKENS,
    };
    tokens * 4
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

impl Agent {
    /// Compact if the last prompt crossed this agent's budget.
    ///
    /// Called at the top of the tool loop, which is the only point in a run
    /// where every `tool_use` already has its `tool_result` — and, because a
    /// fresh turn is iteration 0, is also the turn boundary. There is no second
    /// mechanism for "compact between turns".
    ///
    /// Silent when there is no policy, no budget, or room left.
    pub(crate) async fn maybe_compact(&mut self, current_tokens: u32, events: &dyn EventSink) {
        let Some(budget) = self.config.compaction else {
            return;
        };
        if self.compaction.is_none() || current_tokens < budget.trigger_tokens() {
            return;
        }
        // A failure here is not a failure of the turn. The run continues on the
        // history it already had, which may then overflow and say so — an
        // honest error beats silently proceeding degraded, and retrying belongs
        // to the turn's own budget, not to this.
        if let Err(e) = self
            .compact(None, CompactionTrigger::Auto(EmptyOutcome {}), events)
            .await
        {
            tracing::warn!(error = %e, "a compaction failed; the turn continues uncompacted");
        }
    }

    /// Compact on demand, outside any turn — what `/compact` runs.
    ///
    /// Separate from [`Self::run`] because a `/compact` is not a turn: there is
    /// nothing to say to the model, and starting one would spend a provider
    /// call answering a message nobody sent.
    pub async fn compact_only(
        &mut self,
        instructions: Option<String>,
        events: &dyn EventSink,
    ) -> Result<(), AgentError> {
        self.compact(
            instructions,
            CompactionTrigger::Manual(EmptyOutcome {}),
            events,
        )
        .await
    }

    /// Summarise this agent's whole history, changing nothing.
    ///
    /// What `/summary-n-fork` runs. Distinct from [`Self::compact`] in the one
    /// way that matters: no boundary is written and `self.history` is
    /// untouched, because the session being summarised is not the one that
    /// receives the summary. Folding it back would make the command do two
    /// things, only one of which was asked for.
    ///
    /// Needs no [`CompactionPolicy`] for the same reason: nothing here is
    /// carried into a rewritten history, so there is no state to render and no
    /// hook with an opinion about a rewrite that is not happening.
    ///
    /// # Errors
    /// Whatever the summarising provider call fails with. An empty history
    /// summarises to nothing rather than erroring — a sub session branched
    /// from a session that has not started yet is empty, not broken.
    pub async fn summarise_all(&self, instructions: Option<&str>) -> Result<String, AgentError> {
        if self.history.is_empty() {
            return Ok(String::new());
        }
        self.summarise(self.history.len(), instructions).await
    }

    /// Summarise everything before the cut and rewrite the history.
    ///
    /// `Ok(())` with nothing done when there is no policy or nothing worth
    /// compacting — a `/compact` on a session with two messages in it is a
    /// no-op, not an error.
    pub async fn compact(
        &mut self,
        instructions: Option<String>,
        trigger: CompactionTrigger,
        events: &dyn EventSink,
    ) -> Result<(), AgentError> {
        let Some(policy) = self.compaction.clone() else {
            self.report_nothing_to_fold(trigger, events).await?;
            return Ok(());
        };
        let retain = self.config.compaction.map_or(0, |b| b.retain_tokens());
        let cut = choose_cut(&self.history, retain);
        if cut == 0 {
            // Nothing would be folded away. Compacting here would spend a
            // provider call to replace the history with a summary of itself,
            // and trade real messages for that summary to buy room that was
            // never scarce — so it says so instead of doing it.
            self.report_nothing_to_fold(trigger, events).await?;
            return Ok(());
        }
        let tokens_before = self.last_context_tokens;
        let trigger_name = match trigger {
            CompactionTrigger::Auto(_) => "auto",
            CompactionTrigger::Manual(_) => "manual",
        };
        let plan = CompactionPlan {
            covered: cut,
            retained: self.history.len() - cut,
            tokens_before,
            instructions: instructions.clone(),
            trigger: trigger_name,
        };
        if let PreCompactDecision::Abandon(reason) = policy.before(&plan).await {
            tracing::info!(reason, "a PreCompact hook abandoned this compaction");
            return Ok(());
        }

        let summary = self.summarise(cut, instructions.as_deref()).await?;
        let carried_state = policy.carried_state().await;

        // The name the fold will resolve. A message id, not the index `cut`:
        // the run's history and the actor's log are numbered differently, and
        // the id is what they share.
        let retained_from_message_id = self.history.get(cut).map(|m| m.id.clone());

        let retained: Vec<Message> = self.history[cut..].to_vec();
        let mut rewritten = Vec::with_capacity(retained.len() + 1);
        rewritten.push(Message {
            id: format!("compaction:{}", self.history.len()),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart {
                text: boundary_text(&summary, &carried_state),
            })],
            created_at_ms: now_ms(),
            started_at_ms: None,
        });
        rewritten.extend(retained);
        let tokens_after: u32 = rewritten.iter().map(approx_tokens).sum();
        self.history = rewritten;
        // The next iteration's check reads this. Left at the pre-compaction
        // size it would compact again immediately, every iteration, forever.
        self.last_context_tokens = tokens_after;

        events
            .emit(AgentEvent::Compacted(CompactedEvent {
                message_id: uuid::Uuid::new_v4().to_string(),
                summary: summary.clone(),
                carried_state,
                retained_from_message_id,
                trigger,
                instructions,
                tokens_before,
                tokens_after,
                at_ms: now_ms(),
            }))
            .await?;

        policy
            .after(&CompactionResult {
                summary,
                tokens_before,
                tokens_after,
                trigger: trigger_name,
            })
            .await;
        Ok(())
    }

    /// Say that a compaction found nothing to fold — but only for one that was
    /// asked for.
    ///
    /// An automatic compaction is checked on every iteration of every tool
    /// loop and declines almost all of them; announcing those would bury a
    /// transcript in notices about work that was correctly not done. A typed
    /// `/compact` is a question from a person, and silence was the whole bug.
    async fn report_nothing_to_fold(
        &self,
        trigger: CompactionTrigger,
        events: &dyn EventSink,
    ) -> Result<(), AgentError> {
        if matches!(trigger, CompactionTrigger::Auto(_)) {
            return Ok(());
        }
        events
            .emit(AgentEvent::CompactionSkipped(CompactionSkippedEvent {
                detail: CompactionSkippedLifecycle {
                    context_tokens: self.last_context_tokens,
                    retain_tokens: self.config.compaction.map(|b| b.retain_tokens()),
                },
                at_ms: now_ms(),
            }))
            .await?;
        Ok(())
    }

    /// Ask the model to summarise `history[..cut]`.
    async fn summarise(
        &self,
        cut: usize,
        instructions: Option<&str>,
    ) -> Result<String, AgentError> {
        let mut messages = self.history[..cut].to_vec();
        messages.push(Message {
            id: format!("compaction-request:{cut}"),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart {
                text: summary_prompt(instructions),
            })],
            created_at_ms: now_ms(),
            started_at_ms: None,
        });
        let response = self
            .provider
            .complete(
                CompletionRequest {
                    messages: &messages,
                    // No system prompt: the workspace and tool guidance are
                    // instructions for doing the work, and this call is not
                    // doing the work. They would only bias the summary.
                    system: None,
                    // No tools, which is also what makes `tool_choice`
                    // irrelevant — every provider omits it when tools are
                    // empty.
                    tools: Vec::new(),
                    tool_choice: ToolChoice::Auto,
                    max_tokens: self.config.max_tokens,
                    thinking_effort: None,
                    conversation_id: &self.conversation_id,
                },
                "compaction",
                &NullSink,
            )
            .await
            .map_err(AgentError::Provider)?;

        let text: String = response
            .parts
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
            .join("");
        if text.trim().is_empty() {
            return Err(AgentError::Provider(crate::error::LlmError::ApiError {
                status: 502,
                message: "the summariser returned no text".into(),
            }));
        }
        Ok(text)
    }
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
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
                artifacts: Vec::new(),
            })],
            created_at_ms: 0,
            started_at_ms: None,
        }
    }

    // --- The loop's own behaviour ------------------------------------------

    use crate::testkit::{CollectingEventSink, MockProvider};
    use crate::tool::EmptyToolbox;
    use horsie_models::agent::{AgentInput, Usage};
    use std::sync::{Arc, Mutex};

    /// Records what it was asked and answers with a fixed block of state.
    struct RecordingPolicy {
        carried: String,
        decision: PreCompactDecision,
        plans: Mutex<Vec<CompactionPlan>>,
        results: Mutex<Vec<CompactionResult>>,
    }

    impl RecordingPolicy {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                carried: "TASKS: 1. ship it (in_progress)".into(),
                decision: PreCompactDecision::Proceed,
                plans: Mutex::new(Vec::new()),
                results: Mutex::new(Vec::new()),
            })
        }

        fn refusing() -> Arc<Self> {
            Arc::new(Self {
                carried: String::new(),
                decision: PreCompactDecision::Abandon("a hook said no".into()),
                plans: Mutex::new(Vec::new()),
                results: Mutex::new(Vec::new()),
            })
        }

        fn results(&self) -> usize {
            self.results.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl CompactionPolicy for RecordingPolicy {
        async fn carried_state(&self) -> String {
            self.carried.clone()
        }
        async fn before(&self, plan: &CompactionPlan) -> PreCompactDecision {
            self.plans.lock().unwrap().push(plan.clone());
            self.decision.clone()
        }
        async fn after(&self, result: &CompactionResult) {
            self.results.lock().unwrap().push(result.clone());
        }
    }

    fn budget(window: u32) -> CompactionBudget {
        CompactionBudget {
            context_window: window,
            trigger_at_percent: 80,
            retain_percent: 20,
        }
    }

    /// A provider that reports a large prompt on its first answer — enough to
    /// cross the budget — then a small one.
    fn provider_reporting(input_tokens: &[u32]) -> Arc<MockProvider> {
        let steps: Vec<_> = input_tokens
            .iter()
            .map(|t| {
                Ok(crate::provider::CompletionResponse {
                    parts: vec![ContentPart::Text(TextPart {
                        text: "an answer".into(),
                    })],
                    stop_reason: crate::provider::StopReason::EndTurn,
                    usage: Usage::without_cache(*t, 5),
                })
            })
            .collect();
        MockProvider::scripted(crate::testkit::Script::of(steps).then_repeating_with(|| {
            Ok(crate::provider::CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "a summary of what came before".into(),
                })],
                stop_reason: crate::provider::StopReason::EndTurn,
                usage: Usage::without_cache(10, 5),
            })
        }))
    }

    fn long_history(turns: usize) -> Vec<Message> {
        let mut history = Vec::new();
        for i in 0..turns {
            history.push(user(&format!("u{i}"), &"question ".repeat(200)));
            history.push(msg(
                Role::Assistant,
                &format!("a{i}"),
                &"answer ".repeat(200),
            ));
        }
        history
    }

    async fn run_once(agent: &mut crate::agent::Agent, sink: &CollectingEventSink) {
        let _ = agent
            .run(
                AgentInput::user_message("in-1", "carry on"),
                sink,
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
    }

    fn compactions(sink: &CollectingEventSink) -> Vec<CompactedEvent> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Compacted(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn skips(sink: &CollectingEventSink) -> Vec<CompactionSkippedEvent> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::CompactionSkipped(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_run_under_the_threshold_never_compacts() {
        let policy = RecordingPolicy::new();
        let mut agent =
            crate::agent::Agent::builder(provider_reporting(&[10]), Arc::new(EmptyToolbox), "conv")
                .with_config(crate::AgentConfig {
                    compaction: Some(budget(100_000)),
                    ..Default::default()
                })
                .with_history(long_history(3))
                .with_compaction(policy.clone())
                .with_context_tokens(10)
                .build()
                .unwrap();

        let sink = CollectingEventSink::new();
        run_once(&mut agent, &sink).await;

        assert!(compactions(&sink).is_empty(), "there was room to spare");
        assert_eq!(policy.results(), 0);
    }

    #[tokio::test]
    async fn an_agent_with_no_policy_never_compacts() {
        let mut agent =
            crate::agent::Agent::builder(provider_reporting(&[10]), Arc::new(EmptyToolbox), "conv")
                .with_config(crate::AgentConfig {
                    // A budget that is certainly crossed…
                    compaction: Some(budget(100)),
                    ..Default::default()
                })
                .with_history(long_history(3))
                // …and no policy to carry state or fire hooks.
                .with_context_tokens(10_000)
                .build()
                .unwrap();

        let sink = CollectingEventSink::new();
        run_once(&mut agent, &sink).await;

        assert!(
            compactions(&sink).is_empty(),
            "without a policy there is nobody to ask for carried state, so \
             compacting would silently drop it"
        );
    }

    #[tokio::test]
    async fn crossing_the_threshold_compacts_before_the_next_call() {
        let policy = RecordingPolicy::new();
        // Every call answers the same way, so the assertion below does not
        // depend on whether the summarising call or the turn's own call came
        // first — it is the summarising one, but that is what the *next* test
        // pins down.
        let provider = MockProvider::text("a summary of what came before");
        let mut agent =
            crate::agent::Agent::builder(provider.clone(), Arc::new(EmptyToolbox), "conv")
                .with_config(crate::AgentConfig {
                    compaction: Some(budget(1_000)),
                    ..Default::default()
                })
                .with_history(long_history(4))
                .with_compaction(policy.clone())
                // Seeded above 80% of 1000 — the previous turn left the
                // context full, so iteration 0 of this fresh turn must
                // compact.
                .with_context_tokens(900)
                .build()
                .unwrap();

        let sink = CollectingEventSink::new();
        run_once(&mut agent, &sink).await;

        let events = compactions(&sink);
        assert_eq!(events.len(), 1, "exactly one boundary, at iteration 0");
        let entry = &events[0];
        assert!(entry.summary.contains("a summary of what came before"));
        assert!(
            entry.carried_state.contains("ship it"),
            "the policy's exact state rides on the boundary, got {:?}",
            entry.carried_state
        );
        assert!(entry.tokens_after < entry.tokens_before);
        assert_eq!(policy.results(), 1, "PostCompact saw it");
        assert!(
            provider.calls() >= 2,
            "one call to summarise, then the turn's own — the summary is not \
             free and must not be confused with the answer"
        );
        // The summarising call is the first one, and it carries no tools: it is
        // not a turn and must not be able to start one.
        assert_eq!(
            provider.requests()[0].message_count,
            8 + 1,
            "the covered span plus the ask"
        );
    }

    #[tokio::test]
    async fn the_history_after_a_compaction_is_balanced_and_starts_with_the_summary() {
        let policy = RecordingPolicy::new();
        let mut agent =
            crate::agent::Agent::builder(provider_reporting(&[10]), Arc::new(EmptyToolbox), "conv")
                .with_config(crate::AgentConfig {
                    compaction: Some(budget(1_000)),
                    ..Default::default()
                })
                .with_history(long_history(4))
                .with_compaction(policy.clone())
                .with_context_tokens(900)
                .build()
                .unwrap();

        let sink = CollectingEventSink::new();
        run_once(&mut agent, &sink).await;

        let history = agent.history_for_test();
        assert_eq!(history[0].role, Role::User);
        assert!(
            history[0]
                .parts
                .iter()
                .any(|p| matches!(p, ContentPart::Text(t) if t.text.contains("was compacted"))),
            "the rewritten history opens with the boundary message"
        );
        // Every tool result in what survives must have its call above it.
        let calls: Vec<&str> = history
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolCall(c) => Some(c.id.as_str()),
                _ => None,
            })
            .collect();
        for part in history.iter().flat_map(|m| m.parts.iter()) {
            if let ContentPart::ToolResult(r) = part {
                assert!(
                    calls.contains(&r.tool_call_id.as_str()),
                    "a retained tool result lost its call: {}",
                    r.tool_call_id
                );
            }
        }
    }

    #[tokio::test]
    async fn a_precompact_hook_that_refuses_abandons_the_compaction() {
        let policy = RecordingPolicy::refusing();
        let mut agent =
            crate::agent::Agent::builder(provider_reporting(&[10]), Arc::new(EmptyToolbox), "conv")
                .with_config(crate::AgentConfig {
                    compaction: Some(budget(1_000)),
                    ..Default::default()
                })
                .with_history(long_history(4))
                .with_context_tokens(900)
                .build()
                .unwrap();

        let sink = CollectingEventSink::new();
        run_once(&mut agent, &sink).await;

        assert!(
            compactions(&sink).is_empty(),
            "a refused compaction leaves no boundary"
        );
        assert_eq!(policy.results(), 0, "and never reaches PostCompact");
    }

    /// The turn must survive a summariser that will not answer. Losing the
    /// compaction is a degraded turn; failing the turn is a lost one.
    #[tokio::test]
    async fn a_failing_summariser_leaves_the_run_untouched() {
        let policy = RecordingPolicy::new();
        // Every call fails, including the summarising one.
        let provider = MockProvider::scripted(
            crate::testkit::Script::of([Err(crate::error::LlmError::Overloaded)])
                .then_repeating_with(|| {
                    Ok(crate::provider::CompletionResponse {
                        parts: vec![ContentPart::Text(TextPart { text: "ok".into() })],
                        stop_reason: crate::provider::StopReason::EndTurn,
                        usage: Usage::without_cache(10, 5),
                    })
                }),
        );
        let before = long_history(4);
        let mut agent = crate::agent::Agent::builder(provider, Arc::new(EmptyToolbox), "conv")
            .with_config(crate::AgentConfig {
                compaction: Some(budget(1_000)),
                ..Default::default()
            })
            .with_history(before.clone())
            .with_compaction(policy.clone())
            .with_context_tokens(900)
            .build()
            .unwrap();

        let sink = CollectingEventSink::new();
        run_once(&mut agent, &sink).await;

        assert!(compactions(&sink).is_empty(), "no boundary was written");
        let history = agent.history_for_test();
        assert!(
            history.len() > before.len(),
            "the run went ahead on the history it already had (plus its input)"
        );
        assert_eq!(policy.results(), 0);
    }

    /// The regression: `/compact` used to do nothing *and say nothing*.
    ///
    /// A session inside the retain budget has no span to fold, which is most
    /// sessions and exactly when someone reaches for the command. Declining is
    /// right — compacting would trade real messages for a summary to buy room
    /// that was never scarce — but declining in silence is not, and that is
    /// what this pins.
    #[tokio::test]
    async fn a_manual_compaction_with_nothing_to_fold_says_so() {
        let policy = RecordingPolicy::new();
        let mut agent = crate::agent::Agent::builder(
            MockProvider::text("a summary that must never be asked for"),
            Arc::new(EmptyToolbox),
            "conv",
        )
        .with_config(crate::AgentConfig {
            // A window so large that the whole history is a rounding error
            // against 20% of it — the shape of every real session.
            compaction: Some(budget(1_000_000)),
            ..Default::default()
        })
        .with_history(long_history(3))
        .with_compaction(policy.clone())
        .with_context_tokens(4_000)
        .build()
        .unwrap();

        let sink = CollectingEventSink::new();
        agent.compact_only(None, &sink).await.unwrap();

        assert!(
            compactions(&sink).is_empty(),
            "there was room to spare, so nothing should have been folded"
        );
        let skipped = skips(&sink);
        assert_eq!(skipped.len(), 1, "and the command must be answered");
        assert_eq!(skipped[0].detail.context_tokens, 4_000);
        assert_eq!(
            skipped[0].detail.retain_tokens,
            Some(200_000),
            "the notice says what the conversation was measured against"
        );
        assert_eq!(policy.results(), 0, "no summarising call was made");
        assert_eq!(
            agent.history_for_test().len(),
            long_history(3).len(),
            "and the history is untouched"
        );
    }

    /// An automatic compaction declines on nearly every tool-loop iteration.
    /// Announcing those would bury the transcript in notices about work that
    /// was correctly not done.
    #[tokio::test]
    async fn an_automatic_compaction_with_nothing_to_fold_stays_quiet() {
        let policy = RecordingPolicy::new();
        let mut agent =
            crate::agent::Agent::builder(provider_reporting(&[10]), Arc::new(EmptyToolbox), "conv")
                .with_config(crate::AgentConfig {
                    compaction: Some(budget(1_000_000)),
                    ..Default::default()
                })
                .with_history(long_history(3))
                .with_compaction(policy.clone())
                .with_context_tokens(10)
                .build()
                .unwrap();

        let sink = CollectingEventSink::new();
        run_once(&mut agent, &sink).await;

        assert!(compactions(&sink).is_empty());
        assert!(skips(&sink).is_empty(), "an automatic decline is not news");
    }

    /// A model card with no context window gives no budget to measure against.
    /// The command is still answered — with the part that is knowable.
    #[tokio::test]
    async fn a_manual_compaction_with_no_budget_reports_no_retain_figure() {
        let mut agent = crate::agent::Agent::builder(
            MockProvider::text("a summary that must never be asked for"),
            Arc::new(EmptyToolbox),
            "conv",
        )
        .with_config(crate::AgentConfig {
            compaction: None,
            ..Default::default()
        })
        .with_history(vec![user("u0", "hello"), msg(Role::Assistant, "a0", "hi")])
        .with_compaction(RecordingPolicy::new())
        .with_context_tokens(120)
        .build()
        .unwrap();

        let sink = CollectingEventSink::new();
        agent.compact_only(None, &sink).await.unwrap();

        let skipped = skips(&sink);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].detail.context_tokens, 120);
        assert_eq!(skipped[0].detail.retain_tokens, None);
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
