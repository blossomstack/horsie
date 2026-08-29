use crate::tool::ToolOutcome;
use crate::{
    error::{AgentBuildError, AgentError},
    events::EventSink,
    provider::{CompletionRequest, LlmProvider, StopReason, ToolChoice},
    tool::Toolbox,
};
use horsie_models::agent::{
    AgentInput, AgentOutput, AgentResult, CompletedOutput, ContentPart, Message, Role, StoppedCall,
    StoppedOutput, ToolResultPart, Usage,
};
use horsie_models::events::{
    AgentEvent, InputMessageEvent, MessageCompleteEvent, MessageStartEvent, MessageStopEvent,
    RunAbortedEvent, RunCompleteEvent, ToolCompleteEvent, ToolExecutingEvent,
};
use horsie_models::now_ms;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_iterations: u32,
    pub stuck_threshold: usize,
    pub nudge_threshold: usize,
    pub max_tokens: Option<u32>,
    /// Canonical thinking effort for this run; `None` sends no control.
    pub thinking_effort: Option<crate::thinking::ThinkingEffort>,
    /// How much context this agent has, and when to compact it. `None` means it
    /// never compacts on its own — see [`crate::CompactionBudget`].
    pub compaction: Option<crate::compaction::CompactionBudget>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 100,
            stuck_threshold: 5,
            nudge_threshold: 3,
            max_tokens: None,
            thinking_effort: None,
            compaction: None,
        }
    }
}

pub struct Agent {
    pub(crate) provider: Arc<dyn LlmProvider>,
    /// Which conversation these turns belong to, supplied by the caller rather
    /// than inferred from the history. Providers that can group requests — the
    /// Responses wire uses it as a prompt-cache key — need an id that is the
    /// same across a conversation's turns and different across conversations;
    /// deriving one from message contents breaks the moment history is copied
    /// (a branch) or trimmed (compaction).
    pub(crate) conversation_id: String,
    pub(crate) system_prompt: String,
    pub(crate) toolbox: Arc<dyn Toolbox>,
    /// What this run tells the provider about tool use. `Auto` for every
    /// ordinary run: a turn may end with text, which is how an agent waiting on
    /// a timer or a subagent stops without finishing. A caller nudging an agent
    /// that ended without producing its result passes
    /// `Required("submit_result")`, which is safe only because that agent has
    /// already declared itself done.
    pub(crate) tool_choice: ToolChoice,
    pub(crate) config: AgentConfig,
    pub(crate) history: Vec<Message>,
    /// Supplies what compaction needs and the agent cannot know. `None` means
    /// this agent never compacts.
    pub(crate) compaction: Option<std::sync::Arc<dyn crate::compaction::CompactionPolicy>>,
    /// The last prompt size this agent knows about — the provider's own figure
    /// once a call has answered, and before that whatever the caller seeded from
    /// durable state. Seeding it is what makes iteration 0 of a fresh turn test
    /// the size the *previous* turn left behind, so a turn boundary needs no
    /// separate compaction check.
    pub(crate) last_context_tokens: u32,
}

pub struct AgentBuilder {
    provider: Arc<dyn LlmProvider>,
    /// Identifies the conversation these turns belong to; see [`Agent`].
    conversation_id: String,
    system_prompt: String,
    toolbox: Arc<dyn Toolbox>,
    tool_choice: ToolChoice,
    config: AgentConfig,
    history: Vec<Message>,
    compaction: Option<std::sync::Arc<dyn crate::compaction::CompactionPolicy>>,
    last_context_tokens: u32,
}

impl AgentBuilder {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        toolbox: Arc<dyn Toolbox>,
        conversation_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            conversation_id: conversation_id.into(),
            system_prompt: String::new(),
            toolbox,
            tool_choice: ToolChoice::Auto,
            config: AgentConfig::default(),
            history: Vec::new(),
            compaction: None,
            last_context_tokens: 0,
        }
    }

    /// Supply the owner's half of compaction. Without this an agent never
    /// compacts, whatever its config says.
    pub fn with_compaction(
        mut self,
        policy: std::sync::Arc<dyn crate::compaction::CompactionPolicy>,
    ) -> Self {
        self.compaction = Some(policy);
        self
    }

    /// Seed the prompt size this agent starts believing it has, from durable
    /// state. Without it every turn begins thinking its context is empty and a
    /// session only ever compacts mid-turn.
    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        self.last_context_tokens = tokens;
        self
    }

    pub fn with_system_prompt(mut self, p: impl Into<String>) -> Self {
        self.system_prompt = p.into();
        self
    }

    /// Override what this run tells the provider about tool use. Defaults to
    /// `Auto`, which is right for every ordinary run — a turn ending with text
    /// is legitimate, and which tools may end a run is the toolbox's business,
    /// not the provider's.
    ///
    /// `Required(name)` is for one case only: re-running a turn that ended
    /// without the result it owed. It cannot be used from the start, because it
    /// applies to *every* call in the loop — an agent forced to submit on its
    /// first iteration submits having done no work.
    pub fn with_tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = choice;
        self
    }

    pub fn with_config(mut self, c: AgentConfig) -> Self {
        self.config = c;
        self
    }

    pub fn with_history(mut self, h: Vec<Message>) -> Self {
        self.history = h;
        self
    }

    pub fn build(self) -> Result<Agent, AgentBuildError> {
        if self.config.nudge_threshold >= self.config.stuck_threshold {
            return Err(AgentBuildError::InvalidConfig {
                nudge: self.config.nudge_threshold,
                stuck: self.config.stuck_threshold,
            });
        }

        Ok(Agent {
            provider: self.provider,
            conversation_id: self.conversation_id,
            system_prompt: self.system_prompt,
            toolbox: self.toolbox,
            tool_choice: self.tool_choice,
            config: self.config,
            history: self.history,
            compaction: self.compaction,
            last_context_tokens: self.last_context_tokens,
        })
    }
}

fn extract_tool_calls(parts: &[ContentPart]) -> Vec<(String, String, Value)> {
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

fn extract_text(parts: &[ContentPart]) -> String {
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

fn tool_fingerprint(tool_calls: &[(String, String, Value)]) -> String {
    tool_calls
        .iter()
        .map(|(_, name, input)| format!("{name}:{input}"))
        .collect::<Vec<_>>()
        .join("|")
}

/// Sums two optional per-turn cache-token counts. Stays `None` only when
/// *neither* side reported anything — a turn/provider that's silent about
/// cache data shouldn't zero out a total another turn already contributed to.
fn sum_optional(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// What a run has spent so far.
///
/// Owned by [`Agent::run`] rather than by the loop it drives, so that every one
/// of the loop's exits — including the error returns, which carry nothing —
/// leaves the figures somewhere the caller can still read them. Before this,
/// a cancelled or failed run's tokens existed only in a local the `?` walked
/// past, and the turn reported having cost nothing.
///
/// `context_tokens` is the *last* call's prompt size alone, overwritten rather
/// than summed: it is what is loaded in the model's context, not what the run
/// cost. Both stay zero for a run cancelled before its first call answered.
#[derive(Debug, Clone)]
struct RunAccounting {
    usage: Usage,
    context_tokens: u32,
}

impl RunAccounting {
    fn new() -> Self {
        Self {
            usage: Usage::without_cache(0, 0),
            context_tokens: 0,
        }
    }
}

impl Agent {
    /// `conversation_id` names the conversation these turns belong to. It is a
    /// constructor argument rather than an optional setter on purpose: an
    /// `Agent` is rebuilt for every run, so any default would hand a *different*
    /// id to each turn of one conversation and silently defeat the prompt
    /// caching it exists to enable. Callers pass the agent's own identity.
    pub fn builder(
        provider: Arc<dyn LlmProvider>,
        toolbox: Arc<dyn Toolbox>,
        conversation_id: impl Into<String>,
    ) -> AgentBuilder {
        AgentBuilder::new(provider, toolbox, conversation_id)
    }

    /// The messages this agent would send a provider right now.
    ///
    /// Test-only: the run owns its history and nothing outside it has any
    /// business reading one mid-flight.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn history_for_test(&self) -> &[Message] {
        &self.history
    }

    /// Drive one turn to a conclusion, emitting the run's events as it goes.
    ///
    /// A thin shell around [`Agent::run_inner`], which owns the whole loop. Its
    /// only job is the accounting: every one of the loop's error returns
    /// abandons a run that already spent tokens, and an `AgentError` carries
    /// none of them, so what the run spent is accumulated into a struct this
    /// level owns and reported here as `RunAborted`. Doing it here rather than
    /// before each of the loop's ten-odd `return Err` sites is what makes "every
    /// error path banks its usage" a fact of the code instead of a convention
    /// the next error path has to remember.
    pub async fn run(
        &mut self,
        input: AgentInput,
        events: &dyn EventSink,
        cancel: CancellationToken,
    ) -> Result<AgentOutput, AgentError> {
        let run_id = Uuid::new_v4().to_string();
        let mut spent = RunAccounting::new();
        let result = self
            .run_inner(&run_id, input, events, cancel, &mut spent)
            .await;
        if result.is_err() {
            // The sink's own failure cannot change the outcome — the run has
            // already failed — and propagating it would replace the error that
            // explains the failure with one about bookkeeping.
            let _ = events
                .emit(AgentEvent::RunAborted(RunAbortedEvent {
                    message_id: run_id,
                    usage: spent.usage.clone(),
                    context_tokens: spent.context_tokens,
                    at_ms: now_ms(),
                }))
                .await;
        }
        result
    }

    async fn run_inner(
        &mut self,
        run_id: &str,
        input: AgentInput,
        events: &dyn EventSink,
        cancel: CancellationToken,
        spent: &mut RunAccounting,
    ) -> Result<AgentOutput, AgentError> {
        let input_msg = input.to_message(now_ms());
        events
            .emit(AgentEvent::InputMessage(InputMessageEvent {
                message_id: input.message_id(),
                input,
            }))
            .await?;
        self.history.push(input_msg);

        let mut iteration: u32 = 0;
        let mut recent_fingerprints: VecDeque<String> = VecDeque::new();
        // Whatever the caller chose, for every call this run makes. Honoring
        // `tool_choice` is a hard requirement of every provider.
        let tool_choice = self.tool_choice.clone();

        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            if iteration >= self.config.max_iterations {
                return Err(AgentError::MaxIterationsExceeded {
                    max: self.config.max_iterations,
                });
            }
            iteration += 1;

            // The one compaction check. It sits here because this is the only
            // point in a run where every `tool_use` already has its
            // `tool_result` — and because iteration 0 of a fresh turn is the
            // turn boundary, so "compact between turns" needs no second seam.
            self.maybe_compact(self.last_context_tokens, events).await;

            let tools = self.toolbox.specs();
            let request = CompletionRequest {
                messages: &self.history,
                system: if self.system_prompt.is_empty() {
                    None
                } else {
                    Some(self.system_prompt.clone())
                },
                tools,
                tool_choice: tool_choice.clone(),
                max_tokens: self.config.max_tokens,
                thinking_effort: self.config.thinking_effort,
                conversation_id: &self.conversation_id,
            };

            let msg_id = Uuid::new_v4().to_string();
            events
                .emit(AgentEvent::MessageStart(MessageStartEvent {
                    message_id: msg_id.clone(),
                    role: Role::Assistant,
                }))
                .await?;

            // Cancellation aborts the in-flight completion instead of waiting it
            // out: dropping the provider future tears down the underlying HTTP
            // request/stream, so a stopped turn stops burning tokens now rather
            // than at the next loop checkpoint. Nothing is persisted for an
            // aborted call — only a fully-assembled `MessageComplete` ever is.
            //
            // Stamped here rather than at `MessageStart` so the figure is the
            // provider call's own span: everything between this and the
            // assistant message's `created_at_ms` is generation, and anything
            // outside it is tool or harness time.
            let call_started_ms = now_ms();
            let response = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(AgentError::Cancelled),
                result = self.provider.complete(request, &msg_id, events) => {
                    result.map_err(AgentError::Provider)?
                }
            };

            events
                .emit(AgentEvent::MessageStop(MessageStopEvent {
                    message_id: msg_id.clone(),
                }))
                .await?;

            // Banked the moment the call answers, before anything downstream can
            // fail: from here on every exit — the two `Ok` returns and every
            // `return Err` below — reports this call's cost.
            spent.usage.input_tokens += response.usage.input_tokens;
            spent.usage.output_tokens += response.usage.output_tokens;
            spent.usage.cache_creation_tokens = sum_optional(
                spent.usage.cache_creation_tokens,
                response.usage.cache_creation_tokens,
            );
            spent.usage.cache_read_tokens = sum_optional(
                spent.usage.cache_read_tokens,
                response.usage.cache_read_tokens,
            );
            spent.context_tokens = response.usage.input_tokens;
            self.last_context_tokens = response.usage.input_tokens;

            let assistant_msg = Message {
                id: msg_id.clone(),
                role: Role::Assistant,
                parts: response.parts.clone(),
                created_at_ms: now_ms(),
                started_at_ms: Some(call_started_ms),
            };
            events
                .emit(AgentEvent::MessageComplete(MessageCompleteEvent {
                    message_id: msg_id,
                    message: assistant_msg.clone(),
                }))
                .await?;
            self.history.push(assistant_msg);

            // A truncated turn is not a finished turn. Tool calls are exempt: a
            // backend may report `length` alongside a complete tool call, and the
            // loop can still execute it and continue.
            if response.stop_reason == StopReason::MaxTokens
                && extract_tool_calls(&response.parts).is_empty()
            {
                return Err(AgentError::Truncated {
                    max_tokens: self.config.max_tokens,
                });
            }

            let tool_calls = extract_tool_calls(&response.parts);

            if tool_calls.is_empty() {
                // Plain text is a legitimate way for a turn to end. Whether it
                // *should* have ended that way is the caller's question — only
                // it knows whether a timer or a subagent will wake this agent —
                // so the loop reports the completion and says nothing about it.
                events
                    .emit(AgentEvent::RunComplete(RunCompleteEvent {
                        message_id: run_id.to_string(),
                        usage: spent.usage.clone(),
                        iterations: iteration,
                        context_tokens: spent.context_tokens,
                        at_ms: now_ms(),
                    }))
                    .await?;
                return Ok(AgentOutput {
                    result: AgentResult::Completed(CompletedOutput {
                        text: extract_text(&response.parts),
                    }),
                    usage: spent.usage.clone(),
                });
            }

            let fingerprint = tool_fingerprint(&tool_calls);
            recent_fingerprints.push_back(fingerprint.clone());
            if recent_fingerprints.len() > self.config.stuck_threshold {
                recent_fingerprints.pop_front();
            }

            if recent_fingerprints.len() >= self.config.stuck_threshold
                && recent_fingerprints.iter().all(|f| f == &fingerprint)
            {
                return Err(AgentError::StuckInLoop {
                    tool_name: tool_calls[0].1.clone(),
                    count: self.config.stuck_threshold,
                });
            }

            let should_nudge = recent_fingerprints.len() >= self.config.nudge_threshold
                && recent_fingerprints.iter().all(|f| f == &fingerprint);

            if should_nudge {
                for (tool_call_id, _, _) in &tool_calls {
                    let nudge_msg = Message {
                        id: format!("nudge:{tool_call_id}"),
                        role: Role::Tool,
                        parts: vec![ContentPart::ToolResult(ToolResultPart {
                            tool_call_id: tool_call_id.clone(),
                            output: "You have called this tool with identical arguments multiple times. Please try a different approach.".to_string(),
                            is_error: false,
                            // A nudge is the server talking, not the tool.
                            artifacts: Vec::new(),
                        })],
                        created_at_ms: now_ms(),
                        started_at_ms: None,
                    };
                    self.history.push(nudge_msg);
                }
                continue;
            }

            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            // The whole batch is dispatched, including any call that ends the
            // run. A model may ask a question in the same turn as it starts
            // real work, and the run resumes on this same history — so a
            // dispatched call whose result was never recorded would leave a
            // `tool_use` with no `tool_result`, which every provider rejects.
            let (messages, stopped) = self
                .execute_tool_calls(&tool_calls, events, &cancel)
                .await?;
            for message in messages {
                self.history.push(message);
            }
            if !stopped.is_empty() {
                events
                    .emit(AgentEvent::RunComplete(RunCompleteEvent {
                        message_id: run_id.to_string(),
                        usage: spent.usage.clone(),
                        iterations: iteration,
                        context_tokens: spent.context_tokens,
                        at_ms: now_ms(),
                    }))
                    .await?;
                return Ok(AgentOutput {
                    result: AgentResult::Stopped(StoppedOutput { calls: stopped }),
                    usage: spent.usage.clone(),
                });
            }
        }
    }

    /// Execute `calls` concurrently, emitting each one's start and result, and
    /// return the result messages in request order together with any call that
    /// ended the run.
    ///
    /// The model may request several tools at once (parallel tool use); running
    /// them in parallel cuts a turn's latency to its slowest call rather than the
    /// sum. Request order is preserved so the history stays deterministic.
    ///
    /// A call answering [`ToolOutcome::StopRun`] produces no message and no
    /// `ToolComplete` event: its `tool_use` must stay dangling, because that is
    /// what an answer arrives against later. Recording a result here would give
    /// an answered `ask_user` two of them.
    async fn execute_tool_calls(
        &self,
        calls: &[(String, String, Value)],
        events: &dyn EventSink,
        cancel: &CancellationToken,
    ) -> Result<(Vec<Message>, Vec<StoppedCall>), AgentError> {
        if calls.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let toolbox = &self.toolbox;
        let executions = calls.iter().map(|(tool_call_id, name, input)| async move {
            let result_msg_id = format!("result:{tool_call_id}");

            events
                .emit(AgentEvent::ToolExecuting(ToolExecutingEvent {
                    message_id: result_msg_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                }))
                .await?;

            let (output, is_error, artifacts) =
                match toolbox.execute(name, input.clone(), tool_call_id).await {
                    // A string result is forwarded verbatim; re-encoding it as
                    // JSON would wrap it in quotes and escape every newline,
                    // wasting tokens and hurting readability. Non-string values
                    // are rendered as compact JSON.
                    Ok(ToolOutcome::Result(v)) => (
                        v.value
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.value.to_string()),
                        false,
                        v.artifacts,
                    ),
                    Ok(ToolOutcome::StopRun) => {
                        return Ok(Dispatched::Stopped(StoppedCall {
                            tool: name.clone(),
                            tool_call_id: tool_call_id.clone(),
                            input: input.clone(),
                        }));
                    }
                    // An error produced no artifacts by definition.
                    Err(e) => (e.to_string(), true, Vec::new()),
                };

            // One reading of the clock for both the event and the message it
            // becomes, so the journal and the in-memory history agree.
            let finished_ms = now_ms();
            events
                .emit(AgentEvent::ToolComplete(ToolCompleteEvent {
                    message_id: result_msg_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    output: output.clone(),
                    is_error,
                    at_ms: finished_ms,
                }))
                .await?;

            Ok::<Dispatched, AgentError>(Dispatched::Result(Message {
                id: result_msg_id,
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: tool_call_id.clone(),
                    output,
                    is_error,
                    artifacts,
                })],
                created_at_ms: finished_ms,
                started_at_ms: None,
            }))
        });

        // Cancellation stops *waiting* for the batch (dropping the in-flight
        // tool futures) rather than blocking a stop behind a long-running
        // command. No tool results are recorded for an abandoned batch, so
        // the turn's now-dangling `tool_use` calls are repaired by the
        // caller's resume sanitization before the next run.
        let results = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(AgentError::Cancelled),
            results = futures_util::future::join_all(executions) => results,
        };
        let mut messages = Vec::new();
        let mut stopped = Vec::new();
        for dispatched in results {
            match dispatched? {
                Dispatched::Result(message) => messages.push(message),
                Dispatched::Stopped(call) => stopped.push(call),
            }
        }
        Ok((messages, stopped))
    }
}

/// What one dispatched call produced: a result to record, or the fact that it
/// ended the run.
enum Dispatched {
    Result(Message),
    Stopped(StoppedCall),
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
    use crate::{
        error::ToolCallError,
        events::EventSink,
        provider::{CompletionResponse, StopReason, ToolChoice},
        testkit::{CollectingEventSink, MockProvider, MockToolbox},
        tool::{EmptyToolbox, ToolSpec, Toolbox},
    };
    use async_trait::async_trait;
    use horsie_models::agent::{ContentPart, TextPart, ThinkingPart, ToolCallPart, Usage};
    use horsie_models::events::AgentEvent;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    // --- tests ---

    /// The conversation id the caller named must reach the provider unchanged,
    /// on every turn. Providers that group requests by it — the Responses wire
    /// uses it as a prompt-cache key — get no error if it is wrong, only a
    /// colder cache, so the plumbing is pinned here rather than left to review.
    #[tokio::test]
    async fn the_conversation_id_reaches_the_provider_on_every_turn() {
        let text = |t: &str| CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: t.to_string(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(1, 1),
        };
        let provider = MockProvider::new(vec![text("one"), text("two")]);
        let mut agent = Agent::builder(provider.clone(), Arc::new(EmptyToolbox), "sess-42")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        for msg in ["msg-1", "msg-2"] {
            agent
                .run(
                    AgentInput::user_message(msg, "hi"),
                    &sink,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        let ids: Vec<String> = provider
            .requests()
            .into_iter()
            .map(|r| r.conversation_id)
            .collect();
        assert_eq!(ids, vec!["sess-42".to_string(), "sess-42".to_string()]);
    }

    #[tokio::test]
    async fn a_text_only_turn_completes_and_reports_its_usage() {
        let mut agent = Agent::builder(
            MockProvider::text("Hello, world!"),
            Arc::new(EmptyToolbox),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        let output = agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Completed(CompletedOutput { text }) => assert_eq!(text, "Hello, world!"),
            other => panic!(
                "expected Completed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert_eq!(output.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn completion_emits_message_complete_events() {
        let mut agent = Agent::builder(
            MockProvider::text("done"),
            Arc::new(EmptyToolbox),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            !sink.message_complete_ids().is_empty(),
            "expected at least 1 MessageComplete event, got 0"
        );
    }

    #[tokio::test]
    async fn the_run_emits_the_input_message_it_was_given() {
        let mut agent = Agent::builder(
            MockProvider::text("ok"),
            Arc::new(EmptyToolbox),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "x"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let ie = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::InputMessage(ie) => Some(ie),
                _ => None,
            })
            .unwrap();
        assert_eq!(ie.message_id, "msg-1");
        assert!(matches!(ie.input, AgentInput::UserMessage(_)));
    }

    #[tokio::test]
    async fn a_run_emits_run_complete_exactly_once() {
        let mut agent = Agent::builder(
            MockProvider::text("ok"),
            Arc::new(EmptyToolbox),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "x"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let count = sink
            .events()
            .iter()
            .filter(|e| matches!(e, AgentEvent::RunComplete(_)))
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn a_tool_call_is_executed_and_its_result_finishes_the_turn() {
        let provider =
            MockProvider::tool_then_text("tc1", "search", json!({"q": "rust"}), "found it");
        let toolbox = MockToolbox::echo("search");
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::user_message("msg-1", "search rust"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Completed(CompletedOutput { text }) => assert_eq!(text, "found it"),
            other => panic!(
                "expected Completed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolExecuting(_)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolComplete(_)))
        );
    }

    #[tokio::test]
    async fn every_message_carries_a_server_timestamp() {
        let before = now_ms();
        let provider =
            MockProvider::tool_then_text("tc1", "search", json!({"q": "rust"}), "found it");
        let mut agent = Agent::builder(provider, MockToolbox::echo("search"), "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "search rust"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let after = now_ms();

        assert_eq!(
            agent.history.len(),
            4,
            "user, assistant, tool result, assistant"
        );
        for msg in &agent.history {
            assert!(
                (before..=after).contains(&msg.created_at_ms),
                "{:?} message stamped outside the run",
                msg.role
            );
        }
    }

    #[tokio::test]
    async fn an_assistant_message_reports_when_its_provider_call_began() {
        let mut agent = Agent::builder(
            MockProvider::text("hi"),
            Arc::new(EmptyToolbox),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let assistant = agent
            .history
            .iter()
            .find(|m| m.role == Role::Assistant)
            .cloned()
            .expect("one assistant message");
        let started = assistant.started_at_ms.expect("assistant carries a start");
        assert!(
            started <= assistant.created_at_ms,
            "generation cannot end before it began: {started} > {}",
            assistant.created_at_ms
        );
    }

    #[tokio::test]
    async fn a_tool_result_and_its_event_share_one_stamp() {
        // Two clock readings would let a replayed transcript disagree with the
        // live one about when a tool finished.
        let provider =
            MockProvider::tool_then_text("tc1", "search", json!({"q": "rust"}), "found it");
        let mut agent = Agent::builder(provider, MockToolbox::echo("search"), "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "search rust"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let event_at = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::ToolComplete(ev) => Some(ev.at_ms),
                _ => None,
            })
            .expect("a ToolComplete event");
        let message_at = agent
            .history
            .iter()
            .find(|m| m.role == Role::Tool)
            .map(|m| m.created_at_ms)
            .expect("a tool-result message");
        assert_eq!(event_at, message_at);
    }

    #[tokio::test]
    async fn run_complete_usage_sums_cache_tokens_across_iterations() {
        let provider = MockProvider::new(vec![
            CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "tc1".into(),
                    name: "search".into(),
                    input: json!({}),
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_creation_tokens: Some(15),
                    cache_read_tokens: None,
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 8,
                    cache_creation_tokens: None,
                    cache_read_tokens: Some(25),
                },
            },
        ]);
        let toolbox = MockToolbox::echo("search");
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "x"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let usage = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::RunComplete(rc) => Some(rc.usage),
                _ => None,
            })
            .unwrap();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 18);
        assert_eq!(usage.cache_creation_tokens, Some(15));
        assert_eq!(usage.cache_read_tokens, Some(25));
    }

    #[tokio::test]
    async fn tool_result_message_id_is_derived_from_tool_call_id() {
        let provider = MockProvider::tool_then_text("tc1", "calc", json!({"x": 1}), "result: 1");
        let toolbox = MockToolbox::echo("calc");
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        agent
            .run(
                AgentInput::user_message("msg-1", "calc"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let tc = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::ToolComplete(tc) => Some(tc),
                _ => None,
            })
            .unwrap();
        assert_eq!(tc.message_id, format!("result:{}", tc.tool_call_id));
    }

    #[tokio::test]
    async fn a_stopping_tool_ends_the_run_and_reports_its_call() {
        let provider = MockProvider::new(vec![calls_response(vec![(
            "hc1",
            "stop",
            json!({"answer": 42}),
        )])]);
        let mut agent = Agent::builder(
            provider,
            MockToolbox::with_stopper("stop"),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Stopped(StoppedOutput { calls }) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].tool, "stop");
                assert_eq!(calls[0].tool_call_id, "hc1");
                assert_eq!(calls[0].input["answer"], 42);
            }
            other => panic!("expected Stopped, got {:?}", std::mem::discriminant(&other)),
        }
    }

    /// The dangling `tool_use` *is* the parked state. Recording a result for it
    /// would give an answered `ask_user` two of them, and would make a parked
    /// call indistinguishable from a finished one on reload.
    #[tokio::test]
    async fn a_stopping_call_records_no_tool_result() {
        let provider = MockProvider::new(vec![calls_response(vec![("hc1", "stop", json!({}))])]);
        let mut agent = Agent::builder(
            provider,
            MockToolbox::with_stopper("stop"),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();

        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            tool_results(&sink).is_empty(),
            "no completion event for a call that ended the run"
        );
        let results = agent
            .history_for_test()
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter(|p| matches!(p, ContentPart::ToolResult(_)))
            .count();
        assert_eq!(results, 0, "and nothing in history either");
    }

    /// A tool that ends the run may still refuse the input it was given. That is
    /// an ordinary tool error: the model sees it and re-issues, and nothing about
    /// the run has ended.
    #[tokio::test]
    async fn an_input_error_from_a_stopping_tool_is_an_ordinary_tool_error() {
        let toolbox = MockToolbox::new(
            vec![ToolSpec {
                name: "stop".to_string(),
                description: "stop".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            Arc::new(|_, input: Value| {
                if input.get("answer").is_some() {
                    Ok(ToolOutcome::StopRun)
                } else {
                    Err(ToolCallError::InvalidInput("answer is required".into()))
                }
            }),
        );
        let provider = MockProvider::new(vec![
            calls_response(vec![("h1", "stop", json!({"wrong": true}))]),
            calls_response(vec![("h2", "stop", json!({"answer": 7}))]),
        ]);
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Stopped(StoppedOutput { calls }) => {
                assert_eq!(calls[0].tool_call_id, "h2", "the accepted retry")
            }
            other => panic!("expected Stopped, got {:?}", std::mem::discriminant(&other)),
        }
        let recorded = tool_results(&sink);
        assert_eq!(recorded.len(), 1, "only the rejection: {recorded:?}");
        assert!(recorded[0].2, "and it is an error result");
    }

    /// Every `(tool_call_id, output, is_error)` the sink was told about.
    fn tool_results(sink: &CollectingEventSink) -> Vec<(String, String, bool)> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::ToolComplete(tc) => Some((tc.tool_call_id, tc.output, tc.is_error)),
                _ => None,
            })
            .collect()
    }

    /// A toolbox of `names`, where `stopper` ends the run and everything else
    /// answers "done".
    fn toolbox_where(names: &[&str], stopper: &'static str) -> Arc<MockToolbox> {
        let specs = names
            .iter()
            .map(|n| ToolSpec {
                name: (*n).to_string(),
                description: (*n).to_string(),
                input_schema: json!({ "type": "object" }),
            })
            .collect();
        MockToolbox::new(
            specs,
            Arc::new(move |name: &str, _input| {
                if name == stopper {
                    Ok(ToolOutcome::StopRun)
                } else {
                    Ok(ToolOutcome::result(json!("done")))
                }
            }),
        )
    }

    fn calls_response(calls: Vec<(&str, &str, Value)>) -> CompletionResponse {
        CompletionResponse {
            parts: calls
                .into_iter()
                .map(|(id, name, input)| {
                    ContentPart::ToolCall(ToolCallPart {
                        id: id.to_string(),
                        name: name.to_string(),
                        input,
                    })
                })
                .collect(),
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(1, 1),
        }
    }

    #[tokio::test]
    async fn siblings_execute_before_the_run_stops() {
        // A question is not a conclusion: the run resumes on this very history
        // once the answer arrives, so a tool called in the same turn is real
        // work. It runs and its result is recorded — and it must be, or the
        // resumed conversation carries a `tool_use` with no `tool_result`.
        let provider = MockProvider::new(vec![calls_response(vec![
            ("t1", "notes", json!({"text": "todo"})),
            ("h1", "ask", json!({"question": "which shape?"})),
        ])]);
        let mut agent = Agent::builder(
            provider.clone(),
            toolbox_where(&["notes", "ask"], "ask"),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Stopped(StoppedOutput { calls }) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].tool, "ask");
                assert_eq!(calls[0].input["question"], "which shape?");
            }
            other => panic!("expected Stopped, got {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(
            tool_results(&sink),
            vec![("t1".to_string(), "done".to_string(), false)],
            "the sibling call is executed and journaled; the ask is answered by the user, later"
        );
        assert_eq!(
            provider.calls(),
            1,
            "the turn is honoured as issued — no nudge round trip"
        );
    }

    #[tokio::test]
    async fn several_stopping_calls_are_all_returned() {
        // A park may be issued more than once in a turn: each question is a
        // separate tool call, they are answered together, and the run resumes
        // once every one of them has a result. The loop reports all of them and
        // leaves what that means to the caller.
        let provider = MockProvider::new(vec![calls_response(vec![
            ("h1", "ask", json!({"question": "first?"})),
            ("t1", "notes", json!({})),
            ("h2", "ask", json!({"question": "second?"})),
        ])]);
        let mut agent = Agent::builder(
            provider,
            toolbox_where(&["notes", "ask"], "ask"),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Stopped(StoppedOutput { calls }) => {
                let ids: Vec<&str> = calls.iter().map(|c| c.tool_call_id.as_str()).collect();
                assert_eq!(ids, vec!["h1", "h2"], "in the order the model asked them");
                assert_eq!(calls[0].input["question"], "first?");
                assert_eq!(calls[1].input["question"], "second?");
            }
            other => panic!("expected Stopped, got {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(
            tool_results(&sink),
            vec![("t1".to_string(), "done".to_string(), false)],
            "the sibling still runs; neither ask records a result"
        );
    }

    /// Two different stopping tools in one turn is a contradiction, but not one
    /// the loop can resolve — it does not know what either tool means. Both are
    /// reported, and the caller decides.
    #[tokio::test]
    async fn stopping_calls_to_different_tools_are_both_reported() {
        let provider = MockProvider::new(vec![calls_response(vec![
            ("h1", "submit", json!({"outcome": "success"})),
            ("h2", "ask", json!({"question": "or should I?"})),
        ])]);
        let mut agent = Agent::builder(
            provider,
            MockToolbox::new(
                vec![
                    ToolSpec {
                        name: "submit".into(),
                        description: "submit".into(),
                        input_schema: json!({ "type": "object" }),
                    },
                    ToolSpec {
                        name: "ask".into(),
                        description: "ask".into(),
                        input_schema: json!({ "type": "object" }),
                    },
                ],
                Arc::new(|_, _| Ok(ToolOutcome::StopRun)),
            ),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        match output.result {
            AgentResult::Stopped(StoppedOutput { calls }) => {
                let tools: Vec<&str> = calls.iter().map(|c| c.tool.as_str()).collect();
                assert_eq!(tools, vec!["submit", "ask"]);
            }
            other => panic!("expected Stopped, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn resuming_from_a_tool_result_emits_it_as_the_input() {
        let history = vec![
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "m0".into(),
                role: Role::User,
                parts: vec![ContentPart::Text(TextPart {
                    text: "question".into(),
                })],
            },
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "m1".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "hc1".into(),
                    name: "handoff".into(),
                    input: json!({}),
                })],
            },
        ];
        let provider = MockProvider::text("thanks for the answer");
        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox), "test-conversation")
            .with_history(history)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let output = agent
            .run(
                AgentInput::tool_result("hc1", "42", false),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(matches!(output.result, AgentResult::Completed(_)));

        let ie = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::InputMessage(ie) => Some(ie),
                _ => None,
            })
            .unwrap();
        assert_eq!(ie.message_id, "result:hc1");
        assert!(matches!(ie.input, AgentInput::ToolResult(_)));
    }

    #[tokio::test]
    async fn running_past_max_iterations_is_an_error() {
        let provider = MockProvider::always(CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "t1".into(),
                name: "loop_tool".into(),
                input: json!({}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(5, 2),
        });
        let toolbox = MockToolbox::echo("loop_tool");
        let config = AgentConfig {
            max_iterations: 3,
            stuck_threshold: 10,
            nudge_threshold: 8,
            max_tokens: None,
            ..AgentConfig::default()
        };
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .with_config(config)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let err = agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::MaxIterationsExceeded { max: 3 }));
    }

    #[tokio::test]
    async fn an_agent_going_in_circles_is_stopped_as_stuck() {
        let provider = MockProvider::always(CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "s1".into(),
                name: "stuck_tool".into(),
                input: json!({"x": 1}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(5, 2),
        });
        let toolbox = MockToolbox::echo("stuck_tool");
        let config = AgentConfig {
            max_iterations: 20,
            stuck_threshold: 3,
            nudge_threshold: 2,
            max_tokens: None,
            ..AgentConfig::default()
        };
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .with_config(config)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let err = agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::StuckInLoop { .. }));
    }

    /// A run that ends in an error still reports what it spent. It used to
    /// report nothing at all: the totals lived in a local the error return
    /// walked past, `AgentError` carries no usage, and the only event that
    /// reported any was the one a failing run never emits.
    #[tokio::test]
    async fn a_failed_run_reports_the_tokens_it_spent() {
        let provider = MockProvider::always(CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "t1".into(),
                name: "loop_tool".into(),
                input: json!({}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(5, 2),
        });
        let toolbox = MockToolbox::echo("loop_tool");
        let config = AgentConfig {
            max_iterations: 3,
            stuck_threshold: 10,
            nudge_threshold: 8,
            max_tokens: None,
            ..AgentConfig::default()
        };
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .with_config(config)
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        let aborted: Vec<RunAbortedEvent> = sink
            .events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::RunAborted(ev) => Some(ev),
                _other => None,
            })
            .collect();
        assert_eq!(aborted.len(), 1, "one accounting event ends a run, exactly");
        assert_eq!(aborted[0].usage.input_tokens, 15, "three calls at 5");
        assert_eq!(aborted[0].usage.output_tokens, 6);
        assert_eq!(aborted[0].context_tokens, 5, "the last call's prompt alone");
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::RunComplete(_))),
            "a run ends with one of the two, never both"
        );
    }

    /// Cancelled before the first call answered: nothing was spent, and the
    /// accounting says so rather than being absent.
    #[tokio::test]
    async fn a_run_cancelled_before_it_spent_anything_reports_zero() {
        let provider = MockProvider::text("never reached");
        let toolbox = MockToolbox::echo("some_tool");
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let token = CancellationToken::new();
        token.cancel();

        agent
            .run(AgentInput::user_message("msg-1", "go"), &sink, token)
            .await
            .unwrap_err();

        let aborted = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::RunAborted(ev) => Some(ev),
                _other => None,
            })
            .expect("a cancelled run still accounts for itself");
        assert_eq!(aborted.usage.input_tokens, 0);
        assert_eq!(aborted.context_tokens, 0);
    }

    #[tokio::test]
    async fn a_cancelled_run_ends_as_cancelled() {
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "c1".into(),
                name: "some_tool".into(),
                input: json!({}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(5, 2),
        }]);
        let toolbox = MockToolbox::echo("some_tool");
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let token = CancellationToken::new();
        token.cancel();

        let err = agent
            .run(AgentInput::user_message("msg-1", "go"), &sink, token)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Cancelled));
    }

    /// A provider whose `complete` announces itself and then never resolves, so a
    /// test can cancel while a call is genuinely in flight.
    struct HangingProvider {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
    }

    #[async_trait]
    impl LlmProvider for HangingProvider {
        fn model_id(&self) -> &str {
            "hanging"
        }
        async fn complete(
            &self,
            _request: CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn EventSink,
        ) -> Result<CompletionResponse, crate::error::LlmError> {
            let _ = self.entered.send(());
            std::future::pending().await
        }
    }

    /// A toolbox whose `execute` announces itself and then never resolves — a
    /// stand-in for a long-running command (a slow build, a big test suite).
    struct HangingToolbox {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
    }

    #[async_trait]
    impl Toolbox for HangingToolbox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "slow_tool".to_string(),
                description: "never finishes".to_string(),
                input_schema: json!({ "type": "object" }),
            }]
        }
        async fn execute(
            &self,
            _name: &str,
            _input: Value,
            _tool_call_id: &str,
        ) -> Result<ToolOutcome, ToolCallError> {
            let _ = self.entered.send(());
            std::future::pending().await
        }
    }

    /// Cancelling mid-completion must abort the in-flight provider call rather
    /// than wait it out, so a stopped turn stops burning tokens immediately.
    #[tokio::test]
    async fn cancel_aborts_an_in_flight_provider_call() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut agent = Agent::builder(
            Arc::new(HangingProvider { entered: tx }),
            Arc::new(EmptyToolbox),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        let token = CancellationToken::new();

        // `join!` drives the run and the canceller concurrently: cancel fires only
        // once the provider call has actually been entered, so this exercises the
        // real mid-flight abort with no sleep-guessed timing.
        let run = agent.run(
            AgentInput::user_message("msg-1", "go"),
            &sink,
            token.clone(),
        );
        let canceller = async {
            rx.recv().await.expect("the provider call was entered");
            token.cancel();
        };
        let (result, ()) = tokio::join!(run, canceller);

        assert!(matches!(result.unwrap_err(), AgentError::Cancelled));
        // An aborted call never yields a complete assistant message, so nothing
        // partial can reach the caller's journal.
        assert!(
            sink.message_complete_ids().is_empty(),
            "aborted completion must not produce a MessageComplete"
        );
    }

    /// Cancelling while tools are running must abandon the batch rather than block
    /// the stop behind a long command; no tool results are recorded.
    #[tokio::test]
    async fn cancel_abandons_an_in_flight_tool_batch() {
        // Thinking + a tool call in one assistant message: the shape a real
        // reasoning turn takes, so the assertions below pin down exactly which
        // parts of an interrupted turn survive.
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![
                ContentPart::Thinking(ThinkingPart {
                    text: "let me check...".into(),
                    signature: None,
                }),
                ContentPart::ToolCall(ToolCallPart {
                    id: "c1".into(),
                    name: "slow_tool".into(),
                    input: json!({}),
                }),
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(5, 2),
        }]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut agent = Agent::builder(
            provider,
            Arc::new(HangingToolbox { entered: tx }),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        let token = CancellationToken::new();

        let run = agent.run(
            AgentInput::user_message("msg-1", "go"),
            &sink,
            token.clone(),
        );
        let canceller = async {
            rx.recv().await.expect("the tool call was entered");
            token.cancel();
        };
        let (result, ()) = tokio::join!(run, canceller);

        assert!(matches!(result.unwrap_err(), AgentError::Cancelled));
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolComplete(_))),
            "an abandoned tool batch must not record results"
        );
        // The assistant message *requesting* the tools (thinking included)
        // completed before the batch began, so it is recorded in full — leaving a
        // dangling `tool_use` that the caller's resume sanitization repairs on the
        // next turn. Nothing partial is ever recorded: a message is journaled
        // whole or not at all.
        let completed = sink
            .events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::MessageComplete(mc) => Some(mc.message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1);
        assert!(
            completed[0]
                .parts
                .iter()
                .any(|p| matches!(p, ContentPart::Thinking(_))),
            "the interrupted turn's thinking is preserved with its message"
        );
    }

    /// Records the `tool_choice` of the first provider call.
    struct RecordingProvider {
        seen: Mutex<Option<ToolChoice>>,
        response: CompletionResponse,
    }

    #[async_trait]
    impl crate::provider::LlmProvider for RecordingProvider {
        fn model_id(&self) -> &str {
            "recording"
        }

        async fn complete(
            &self,
            request: crate::provider::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn EventSink,
        ) -> Result<CompletionResponse, crate::error::LlmError> {
            *self.seen.lock().unwrap() = Some(request.tool_choice.clone());
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn a_run_uses_tool_choice_auto_by_default() {
        let provider = Arc::new(RecordingProvider {
            seen: Mutex::new(None),
            response: CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage::without_cache(1, 1),
            },
        });
        let mut agent = Agent::builder(
            provider.clone(),
            Arc::new(EmptyToolbox),
            "test-conversation",
        )
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            provider.seen.lock().unwrap().clone(),
            Some(ToolChoice::Auto)
        ));
    }

    /// The one case for naming a tool: re-running a turn that ended without the
    /// result it owed. It cannot be set from the start — `tool_choice` applies to
    /// every call in the loop, so an agent forced to submit on its first
    /// iteration submits having done no work.
    #[tokio::test]
    async fn a_caller_can_force_one_tool_for_a_whole_run() {
        let provider = Arc::new(RecordingProvider {
            seen: Mutex::new(None),
            response: CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage::without_cache(1, 1),
            },
        });
        let mut agent = Agent::builder(
            provider.clone(),
            MockToolbox::with_stopper("submit_result"),
            "test-conversation",
        )
        .with_tool_choice(ToolChoice::Required("submit_result".into()))
        .build()
        .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            provider.seen.lock().unwrap().clone(),
            Some(ToolChoice::Required(name)) if name == "submit_result"
        ));
    }

    #[tokio::test]
    async fn string_tool_output_is_not_json_escaped() {
        // A tool returning a JSON string should reach the conversation verbatim —
        // no surrounding quotes, no `\n` escapes from a second JSON encoding.
        let provider = MockProvider::tool_then_text("t1", "cat", json!({}), "done");
        let toolbox = MockToolbox::new(
            vec![ToolSpec {
                name: "cat".to_string(),
                description: "cat".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            Arc::new(|_, _| {
                Ok(ToolOutcome::result(Value::String(
                    "line1\nline2".to_string(),
                )))
            }),
        );
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let tc = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::ToolComplete(tc) => Some(tc),
                _ => None,
            })
            .unwrap();
        assert_eq!(tc.output, "line1\nline2");
        assert!(
            !tc.output.contains("\\n"),
            "output was JSON-escaped: {}",
            tc.output
        );
    }

    struct BarrierToolbox {
        barrier: Arc<tokio::sync::Barrier>,
        timed_out: Arc<std::sync::atomic::AtomicBool>,
        spec: ToolSpec,
    }

    #[async_trait]
    impl Toolbox for BarrierToolbox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![self.spec.clone()]
        }

        async fn execute(
            &self,
            _name: &str,
            input: Value,
            _tool_call_id: &str,
        ) -> Result<ToolOutcome, ToolCallError> {
            // Concurrent execution => both calls reach the barrier and proceed at
            // once. Sequential execution => the first call blocks here until the
            // timeout fires, flagging the regression.
            if tokio::time::timeout(std::time::Duration::from_secs(2), self.barrier.wait())
                .await
                .is_err()
            {
                self.timed_out
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(ToolOutcome::result(input))
        }
    }

    #[tokio::test]
    async fn tool_calls_in_a_turn_run_concurrently() {
        let provider = MockProvider::new(vec![
            CompletionResponse {
                parts: vec![
                    ContentPart::ToolCall(ToolCallPart {
                        id: "a".into(),
                        name: "wait".into(),
                        input: json!({}),
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "b".into(),
                        name: "wait".into(),
                        input: json!({}),
                    }),
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage::without_cache(1, 1),
            },
            CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage::without_cache(1, 1),
            },
        ]);
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let toolbox = Arc::new(BarrierToolbox {
            barrier: Arc::new(tokio::sync::Barrier::new(2)),
            timed_out: timed_out.clone(),
            spec: ToolSpec {
                name: "wait".to_string(),
                description: "wait".to_string(),
                input_schema: json!({ "type": "object" }),
            },
        });
        let mut agent = Agent::builder(provider, toolbox, "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("m", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !timed_out.load(std::sync::atomic::Ordering::SeqCst),
            "tool calls in a turn ran sequentially, not concurrently"
        );
    }

    #[tokio::test]
    async fn max_tokens_truncation_is_an_error_not_a_completion() {
        // stop_reason was computed but never read in production: a response cut
        // off by max_tokens looked exactly like a normal end of turn, so the
        // caller received a silently truncated answer as a success.
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: "half an ans".into(),
            })],
            stop_reason: StopReason::MaxTokens,
            usage: Usage::without_cache(10, 5),
        }]);
        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox), "test-conversation")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let err = agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .expect_err("truncation must not be reported as a completed turn");

        assert!(
            matches!(err, AgentError::Truncated { .. }),
            "expected Truncated, got {err:?}",
        );
    }
}
