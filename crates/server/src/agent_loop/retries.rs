//! What one turn does between the actor deciding to run and an outcome coming
//! back.
//!
//! The retry rule is the reason this is worth its own unit. An attempt may only
//! be re-run when nothing durable was written, because a retry rebuilds the turn
//! from the history it started with — so retrying after partial progress leaves
//! a phantom turn in the transcript that the model never saw. That test is made
//! against the same `coarse_event` mapping the journal uses, never a proxy for
//! it.
//!
//! No actor and no state: a turn is given everything it needs by value, which is
//! what lets it run on its own task and be tested without a journal.

use crate::agent_loop::agent_actor::{CapturingSink, ForkSummary, RunOutcome};
use crate::agent_loop::inbox::Summarise;
use crate::agent_loop::state::coarse_event;
#[cfg(test)]
use async_trait::async_trait;
use horsie_agentcore::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentResult, EventSink, LlmProvider,
    Message, Toolbox,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[allow(clippy::too_many_arguments)]
/// A [`CompactionPolicy`](horsie_agentcore::CompactionPolicy) for agents that
/// have no budget, so it is never consulted. Tests that exercise the retry loop
/// need one to pass and nothing to happen.
#[cfg(test)]
struct NeverCompacts;

#[cfg(test)]
#[async_trait]
impl horsie_agentcore::CompactionPolicy for NeverCompacts {
    async fn carried_state(&self) -> String {
        String::new()
    }
    async fn before(
        &self,
        _: &horsie_agentcore::CompactionPlan,
    ) -> horsie_agentcore::PreCompactDecision {
        horsie_agentcore::PreCompactDecision::Proceed
    }
    async fn after(&self, _: &horsie_agentcore::CompactionResult) {}
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_with_retries(
    provider: Arc<dyn LlmProvider>,
    toolbox: Arc<dyn Toolbox>,
    sink: Arc<dyn EventSink>,
    conversation_id: String,
    system_prompt: String,
    tool_choice: Option<horsie_agentcore::ToolChoice>,
    max_iterations: Option<u32>,
    max_retries: u32,
    thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    history: Vec<Message>,
    input: AgentInput,
    cancel: CancellationToken,
    compaction: Option<horsie_agentcore::CompactionBudget>,
    compaction_policy: Arc<dyn horsie_agentcore::CompactionPolicy>,
    context_tokens: u32,
    summarise: Option<Summarise>,
    summarise_only: bool,
) -> (RunOutcome, Option<ForkSummary>) {
    // Whatever a fork is waiting on is taken first, before this turn can say
    // anything to the model: the summary has to describe the history the branch
    // marker was written into, not one this turn went on to extend.
    let (compact, fork_summary) = match summarise {
        Some(Summarise::Compact(instructions)) => (Some(instructions), None),
        Some(Summarise::Fork(forks)) => {
            let result = summarise_for_forks(
                &provider,
                &toolbox,
                &conversation_id,
                &history,
                thinking_effort,
            )
            .await;
            if let Err(e) = &result {
                tracing::warn!(error = %e, "summarising a conversation for a fork failed");
            }
            (None, Some(ForkSummary { forks, result }))
        }
        None => (None, None),
    };
    // A turn whose whole job was that summary is over: there is nothing to send.
    // The compaction case cannot short-circuit here, because it needs the agent
    // the loop below builds.
    if summarise_only && compact.is_none() {
        return (
            RunOutcome::Completed {
                text: String::new(),
            },
            fork_summary,
        );
    }
    (
        run_turn_attempts(
            provider,
            toolbox,
            sink,
            conversation_id,
            system_prompt,
            tool_choice,
            max_iterations,
            max_retries,
            thinking_effort,
            history,
            input,
            cancel,
            compaction,
            compaction_policy,
            context_tokens,
            compact,
            summarise_only,
        )
        .await,
        fork_summary,
    )
}

/// Summarise a conversation for the forks branching off it.
///
/// A throwaway `Agent` over the same provider and history: the summary is a
/// *reading* of this conversation for somebody else, so nothing is journaled,
/// nothing is streamed, and this agent's own history is left exactly as it was.
async fn summarise_for_forks(
    provider: &Arc<dyn LlmProvider>,
    toolbox: &Arc<dyn Toolbox>,
    conversation_id: &str,
    history: &[Message],
    thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
) -> Result<String, String> {
    let agent = Agent::builder(provider.clone(), toolbox.clone(), conversation_id)
        .with_config(AgentConfig {
            thinking_effort,
            ..AgentConfig::default()
        })
        .with_history(history.to_vec())
        .build()
        .map_err(|e| e.to_string())?;
    agent.summarise_all(None).await.map_err(|e| e.to_string())
}

/// The retry loop proper: everything a turn does once its summarisation, if it
/// had one, has been dealt with.
#[allow(clippy::too_many_arguments)]
async fn run_turn_attempts(
    provider: Arc<dyn LlmProvider>,
    toolbox: Arc<dyn Toolbox>,
    sink: Arc<dyn EventSink>,
    conversation_id: String,
    system_prompt: String,
    tool_choice: Option<horsie_agentcore::ToolChoice>,
    max_iterations: Option<u32>,
    max_retries: u32,
    thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    history: Vec<Message>,
    input: AgentInput,
    cancel: CancellationToken,
    compaction: Option<horsie_agentcore::CompactionBudget>,
    compaction_policy: Arc<dyn horsie_agentcore::CompactionPolicy>,
    context_tokens: u32,
    compact: Option<Option<String>>,
    compact_only: bool,
) -> RunOutcome {
    let mut attempt: u32 = 0;
    loop {
        // CapturingSink wraps the PersistSink: it records events only to locate the
        // handoff tool-call id; persistence (with backpressure) happens in PersistSink.
        let capture = CapturingSink::new(sink.clone());
        let config = AgentConfig {
            max_iterations: max_iterations.unwrap_or_else(|| AgentConfig::default().max_iterations),
            thinking_effort,
            compaction,
            ..AgentConfig::default()
        };
        let mut builder = Agent::builder(provider.clone(), toolbox.clone(), &conversation_id)
            .with_system_prompt(system_prompt.clone())
            .with_config(config)
            .with_history(history.clone())
            .with_compaction(compaction_policy.clone())
            .with_context_tokens(context_tokens);
        if let Some(choice) = tool_choice.clone() {
            builder = builder.with_tool_choice(choice);
        }

        let mut agent = match builder.build() {
            Ok(a) => a,
            Err(e) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: false,
                };
            }
        };

        // A queued `/compact` runs before anything this turn says to the model,
        // and when it is the whole turn, instead of it. Not retried: a
        // compaction that failed leaves the history it started with, which is
        // exactly what the turn would have run on anyway.
        if let Some(instructions) = compact.clone() {
            if let Err(e) = agent.compact_only(instructions, &capture).await {
                tracing::warn!(error = %e, "a requested compaction failed");
            }
            if compact_only {
                return RunOutcome::Completed {
                    text: String::new(),
                };
            }
        }

        let result = agent.run(input.clone(), &capture, cancel.clone()).await;
        let captured = capture.take();

        match result {
            Ok(output) => {
                return match output.result {
                    AgentResult::Completed(c) => RunOutcome::Completed { text: c.text },
                    AgentResult::Stopped(s) => RunOutcome::Stopped { calls: s.calls },
                };
            }
            Err(AgentError::Cancelled) => return RunOutcome::Cancelled,
            Err(AgentError::Provider(e)) => {
                // Whether the failed attempt already wrote something durable.
                // `PersistSink` journals exactly the events `coarse_event` maps,
                // so this is the same test it applied — no proxy, no guessing.
                // `RunAborted` is the exception: it is written *by* this
                // failure rather than by anything the attempt achieved, so
                // counting it would make every transient error look like
                // partial progress and no attempt would ever be retried.
                let journaled = captured.iter().any(|ev| {
                    !matches!(ev, AgentEvent::RunAborted(_)) && coarse_event(ev).is_some()
                });
                // Three independent conditions, all required:
                //
                // 1. Budget remains.
                // 2. The failure is transient. `LlmError` already distinguishes
                //    RateLimit / Overloaded / Network from a permanent ApiError,
                //    and this layer used to discard all of it — retrying a 401 or
                //    a 400 context-length error exactly as eagerly as a 429.
                // 3. Nothing durable was written. The retry rebuilds the turn from
                //    the ORIGINAL `history`, which does not contain the events the
                //    failed attempt persisted, so retrying after partial progress
                //    leaves a phantom turn in the transcript that the model never
                //    saw — replayed into every later turn (#61 item 21). This is
                //    the same "only retry when nothing has been emitted" rule the
                //    providers already apply to their own streams.
                if attempt < max_retries && e.is_transient() && !journaled {
                    attempt += 1;
                    // Honour a provider-supplied delay when there is one; the
                    // exponential backoff is the fallback, not the rule.
                    let delay = e
                        .retry_after()
                        .unwrap_or_else(|| Duration::from_millis(50u64 * (1u64 << attempt.min(6))));
                    tracing::warn!(
                        error = %e,
                        attempt,
                        delay_ms = delay.as_millis(),
                        "transient provider error with nothing journaled; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                if journaled && e.is_transient() && attempt < max_retries {
                    tracing::warn!(
                        error = %e,
                        "not retrying: the attempt already journaled progress that a \
                         restart from the original history would duplicate"
                    );
                }
                return RunOutcome::Failed {
                    // Report the classification rather than assuming recoverable:
                    // a permanent failure shown as transient invites the user to
                    // retry something that can never succeed.
                    recoverable: e.is_transient(),
                    error: e.to_string(),
                };
            }
            Err(e) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: false,
                };
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod retry_tests {
    use super::*;
    use horsie_agentcore::EventSinkError;
    use horsie_agentcore::testkit::{
        CollectingEventSink, FailingEventSink, MockProvider, MockToolbox, Script,
    };
    use horsie_agentcore::{
        CompletionResponse, ContentPart, EmptyToolbox, LlmError, StopReason, ToolOutcome, ToolSpec,
    };
    use horsie_models::agent::{TextPart, ToolCallPart, Usage};

    fn text_response(text: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(1, 1),
        }
    }

    fn tool_response(id: &str, name: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(1, 1),
        }
    }

    fn echo_toolbox() -> Arc<MockToolbox> {
        MockToolbox::new(
            vec![ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            Arc::new(|_, input| Ok(ToolOutcome::Result(input))),
        )
    }

    async fn run(
        provider: Arc<MockProvider>,
        toolbox: Arc<dyn Toolbox>,
        max_retries: u32,
    ) -> (RunOutcome, usize) {
        let sink: Arc<dyn EventSink> = Arc::new(CollectingEventSink::new());
        let outcome = run_turn_attempts(
            provider.clone(),
            toolbox,
            sink,
            "test-conversation".to_string(),
            "sys".into(),
            None,
            Some(10),
            max_retries,
            None,
            vec![],
            AgentInput::user_message("m1", "go"),
            CancellationToken::new(),
            // Retry behaviour is what these exercise; compaction is off so a
            // budget can never change how many calls a retry makes.
            None,
            Arc::new(NeverCompacts),
            0,
            None,
            false,
        )
        .await;
        let calls = provider.calls();
        (outcome, calls)
    }

    #[tokio::test]
    async fn a_transient_error_is_retried_when_nothing_was_journaled() {
        let provider = MockProvider::scripted(Script::of([
            Err(LlmError::Overloaded),
            Ok(text_response("second time lucky")),
        ]));
        let (outcome, calls) = run(provider, Arc::new(EmptyToolbox), 1).await;

        assert!(
            matches!(outcome, RunOutcome::Completed { .. }),
            "got {outcome:?}"
        );
        assert_eq!(calls, 2, "the transient failure should have been retried");
    }

    /// The accounting event a failed attempt writes must not be mistaken for
    /// progress. It is written *by* the failure, so counting it as "something
    /// durable was written" would suppress every retry there is.
    #[tokio::test]
    async fn a_runs_own_accounting_does_not_count_as_journaled_progress() {
        let provider = MockProvider::scripted(Script::of([
            Err(LlmError::Overloaded),
            Err(LlmError::Overloaded),
            Ok(text_response("third time lucky")),
        ]));
        let (outcome, calls) = run(provider, Arc::new(EmptyToolbox), 2).await;
        assert!(
            matches!(outcome, RunOutcome::Completed { .. }),
            "got {outcome:?}"
        );
        assert_eq!(calls, 3, "both transient failures should have been retried");
    }

    #[tokio::test]
    async fn a_permanent_error_is_not_retried() {
        // #61 item 21: every AgentError::Provider used to be retried identically,
        // so a 401 or a 400 context-length error burned the whole retry budget.
        let provider = MockProvider::failing(LlmError::ApiError {
            status: 401,
            message: "bad key".into(),
        });
        let (outcome, calls) = run(provider, Arc::new(EmptyToolbox), 3).await;

        assert_eq!(calls, 1, "a permanent error must not be retried");
        match outcome {
            RunOutcome::Failed { recoverable, .. } => assert!(
                !recoverable,
                "a 401 must not be reported to the user as recoverable"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    async fn run_with_sink(
        provider: Arc<MockProvider>,
        sink: Arc<dyn EventSink>,
        max_retries: u32,
    ) -> (RunOutcome, usize) {
        let outcome = run_turn_attempts(
            provider.clone(),
            Arc::new(EmptyToolbox),
            sink,
            "test-conversation".to_string(),
            "sys".into(),
            None,
            Some(10),
            max_retries,
            None,
            vec![],
            AgentInput::user_message("m1", "go"),
            CancellationToken::new(),
            // Retry behaviour is what these exercise; compaction is off so a
            // budget can never change how many calls a retry makes.
            None,
            Arc::new(NeverCompacts),
            0,
            None,
            false,
        )
        .await;
        let calls = provider.calls();
        (outcome, calls)
    }

    /// #61 item 22, half one: the failure raised *inside* `complete()`.
    ///
    /// A journal write failure surfacing through the provider arrives as
    /// `LlmError::EventSink` → `AgentError::Provider`, which this layer used to
    /// retry against the LLM — burning tokens on a disk fault.
    #[tokio::test]
    async fn a_sink_failure_from_the_provider_is_not_retried_against_the_llm() {
        let provider = MockProvider::scripted(Script::of([]).then_repeating_with(|| {
            Err(LlmError::EventSink(EventSinkError(
                "journal write failed: disk full".into(),
            )))
        }));
        let sink: Arc<dyn EventSink> = Arc::new(CollectingEventSink::new());
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(
            calls, 1,
            "a journal failure must not be retried against the LLM"
        );
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(!recoverable, "a disk failure is not a recoverable turn");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// #61 item 22, half two: the same root cause raised by the agent loop's own
    /// `events.emit(...)?`, which becomes `AgentError::EventSink`.
    ///
    /// The issue's complaint was that one root cause got two different verdicts
    /// depending on where it surfaced. Both paths must agree, and neither may
    /// retry against the LLM.
    #[tokio::test]
    async fn a_sink_failure_at_turn_start_costs_no_tokens() {
        // `Agent::run` journals the input message before it ever calls the
        // provider, so a journal that is already down fails the turn for free.
        let provider = MockProvider::text("hello");
        let sink: Arc<dyn EventSink> = Arc::new(FailingEventSink::always("journal write failed"));
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(calls, 0, "the provider must never be reached");
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(!recoverable, "a disk failure is not a recoverable turn");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sink_failure_mid_turn_is_not_retried_and_agrees_with_the_provider_path() {
        // Let the input message and the message-start through, so the provider is
        // genuinely engaged before the journal dies — the realistic shape.
        let provider = MockProvider::text("hello");
        let sink: Arc<dyn EventSink> = Arc::new(FailingEventSink::after(2, "journal write failed"));
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(
            calls, 1,
            "the turn must not be re-run against the LLM after a journal failure"
        );
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(
                    !recoverable,
                    "both sink-failure paths must report the same verdict"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_transient_error_after_journaled_progress_is_not_retried() {
        // The crux of #61 item 21: the retry rebuilds the turn from the ORIGINAL
        // history, which does not contain the events the failed attempt already
        // persisted. Retrying here would leave a phantom turn in the durable
        // transcript that the model never saw, replayed into every later turn.
        let provider = MockProvider::scripted(Script::of([
            Ok(tool_response("call-1", "echo")),
            Err(LlmError::Overloaded),
            Ok(text_response("must never be reached")),
        ]));
        let (outcome, calls) = run(provider, echo_toolbox(), 3).await;

        assert_eq!(
            calls, 2,
            "once a tool result is journaled the turn must not restart from a \
             history that omits it"
        );
        assert!(
            matches!(outcome, RunOutcome::Failed { .. }),
            "got {outcome:?}"
        );
    }
}
