//! What ended the turn, and what that means — the other half of the
//! [`Turn`](super::run::Turn) component.
//!
//! A turn ends one of four ways: a tool stopped it, the model wrote plain text,
//! it was cancelled, or it failed. Only this component can tell a *park* from a
//! mistake, because only it knows what would wake the agent — a pending ask or
//! an armed timer means the silence is deliberate; nothing pending means the
//! turn owed a result and did not produce one.
//!
//! Every ending finishes the same way: the turn's flight is cleared, the
//! queue's gate is lowered, and a `Drain` is told — never a direct call into
//! the queue. Whether anything queued becomes the next turn is the queue's own
//! decision, taken against the state this ending persisted.
//!
//! [`Turn::interpret`] is where the stopping tools are read, and it is
//! deliberately strict: two different finishing tools in one turn is a
//! contradiction, not a preference to resolve.

use super::run::Turn;
use super::*;
use crate::agent_loop::context::{AgentOutcome, AgentOutcomeSink, AskedQuestion};
use crate::agent_loop::queued_turn;
use crate::sessions::ask_tool::ASK_USER_TOOL;
use crate::sessions::workflow::SUBMIT_RESULT_TOOL;
use horsie_actor::CommandEffect;
use horsie_agentcore::StoppedCall;
use horsie_models::now_ms;
use serde_json::Value;
use std::sync::Arc;

/// How many turns an agent that owes a result may end without one before the
/// step is failed. Two: the first nudge is a plain message, the second forces
/// `submit_result` in `tool_choice`, and a model that defeats both is not going
/// to be talked round by a third.
pub(super) const MAX_RESULT_NUDGES: u32 = 2;

#[derive(Debug)]
pub(super) enum Conclusion {
    Output(Value),
    /// One or more questions, all parked on together.
    Ask(Vec<AskedQuestion>),
    /// Two turn-enders at once. The calls are named so each can be told why.
    Contradiction(Vec<StoppedCall>),
}

impl Turn {
    /// Interpret what ended the run — a tool that stopped it, or a plain-text
    /// completion — and deliver the outcome to the parent. The turn's events
    /// were already persisted step by step, so this only records the terminal
    /// transition and lowers the gate.
    pub(super) async fn conclude(
        &mut self,
        report: RunReport,
        state: &AgentState,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        // A report from a turn that has already been superseded says nothing
        // about the one in flight now.
        if self.flight_id() != Some(report.run_id) {
            tracing::warn!(
                run_id = report.run_id,
                current = ?self.flight_id(),
                "dropping the report of a superseded run"
            );
            return CommandEffect::none();
        }
        // The turn is over before anything below is decided: clear the flight
        // and lower the queue's gate, so the drains told below can start the
        // next one.
        self.clear_flight(cx);
        // Answered before any parent delivery below: a canceller is likely
        // blocking its own mailbox waiting on this, and those deliveries
        // `tell` into that same mailbox — replying first keeps the two from
        // deadlocking. The fence guarantees a cancelled turn's tasks can make
        // nothing more durable, so "it will write nothing more" is true now.
        for ack in self.cancel_acks.drain(..) {
            let _ = ack.send(());
        }
        let agent = cx.runtime.journal_id;
        let parent = cx.runtime.parent.clone();

        // Before the turn's own outcome, and unconditionally: the sub sessions
        // waiting on this are a different session's business, and whether this
        // turn then went on to succeed, fail or be cancelled says nothing
        // about whether their summary was taken.
        if let Some(SeedSummary {
            sub_sessions,
            result,
        }) = report.seed_summary
        {
            parent
                .deliver(AgentOutcome::SeedSummary {
                    agent,
                    sub_sessions,
                    result,
                })
                .await;
        }

        match report.outcome {
            RunOutcome::Completed { text } => {
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
                        context_tokens: state.context_tokens,
                    })
                    .await;
                if cx.params.requires_result {
                    return self.ended_without_result(state, cx, agent, parent).await;
                }
                // An agent that owes its parent one report is not done while
                // work it delegated is still running: its conclusion would be
                // consumed now and the children's results would arrive at an
                // agent whose requester already moved on. The queue is checked
                // first — a child's report that landed mid-turn simply starts
                // the next turn — and otherwise the agent parks; the children
                // finishing is what wakes it, and its next conclusion is the
                // report.
                if cx.params.park_on_outstanding_work {
                    if queued_turn(&state.inbox, &state.asks).is_some() {
                        cx.drain().await;
                        return CommandEffect::none();
                    }
                    if !state.timers.is_empty()
                        || crate::agent_loop::carried_state::has_outstanding_children(state)
                    {
                        parent.deliver(AgentOutcome::Parked { agent }).await;
                        let parked = AgentDomainEvent::Parked { at_ms: now_ms() };
                        return CommandEffect::persist(vec![parked]).and_snapshot();
                    }
                }
                parent
                    .deliver(AgentOutcome::Concluded {
                        agent,
                        output: Value::String(text),
                    })
                    .await;
                // Resident: the agent goes idle, it does not die. A turn
                // ending is a boundary, so whatever queued while it ran
                // starts the next one.
                cx.drain().await;
                CommandEffect::none()
            }
            RunOutcome::Stopped { calls } => {
                match Self::interpret(calls) {
                    Conclusion::Output(output) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                                context_tokens: state.context_tokens,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Concluded { agent, output })
                            .await;
                        // Submitting says the work is done, which makes any
                        // armed timer moot: nothing is left for it to wake.
                        // Dropping them here rather than calling it a
                        // contradiction keeps one rule — the agent decides when
                        // it is finished — and avoids a failure mode the agent
                        // could not have been warned about at the tool
                        // boundary, where its own timers are invisible.
                        let mut events = Vec::new();
                        if !state.timers.is_empty() {
                            events.push(AgentDomainEvent::TimerCancelled {
                                ids: state.timers.iter().map(|t| t.id.clone()).collect(),
                                at_ms: now_ms(),
                            });
                        }
                        cx.drain().await;
                        CommandEffect::persist(events)
                    }
                    Conclusion::Ask(asks) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                                context_tokens: state.context_tokens,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Asked {
                                agent,
                                asks: asks.clone(),
                            })
                            .await;
                        // An ask is a turn boundary, but a parked agent only
                        // drains for a person changing their mind — the drain
                        // told here finds the asks folded in and holds
                        // anything queued behind them.
                        let recorded = AgentDomainEvent::AskRecorded {
                            asks,
                            at_ms: now_ms(),
                        };
                        cx.drain().await;
                        // Snapshot to compact the incrementally-persisted log:
                        // history and streams read state, so it is invisible.
                        CommandEffect::persist(vec![recorded]).and_snapshot()
                    }
                    Conclusion::Contradiction(calls) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                                context_tokens: state.context_tokens,
                            })
                            .await;
                        self.correct_contradiction(calls, state, cx).await
                    }
                }
            }
            RunOutcome::Cancelled => {
                // The tokens were spent whatever became of the turn that spent
                // them; the caller banked them as `RunAborted` in the same
                // batch, so the total read here includes them.
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
                        context_tokens: state.context_tokens,
                    })
                    .await;
                // A cancelled tool call has no result and never will get one.
                // Journal the synthetic result now, where it belongs —
                // directly after the assistant message that made the call —
                // rather than recomputing it on a clone at the top of every
                // later turn.
                let mut events: Vec<AgentDomainEvent> =
                    missing_tool_results(&state.prompt_messages(), &parked_call_ids(state))
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message })
                        .collect();
                // Whatever the model had already written is the only copy
                // there is: deltas are unjournaled by design, and the boundary
                // entry the stop is about to append clears them.
                //
                // After the synthetic results, not before: a cancelled call's
                // result belongs directly under the message that made it, and
                // this text is a later message than that one.
                if let Some(salvaged) = Turn::aborted_message(cx) {
                    events.push(AgentDomainEvent::MessageAborted { message: salvaged });
                }
                events.push(AgentDomainEvent::RunCancelled { at_ms: now_ms() });
                // A stop cancels the turn, not the promise: anything queued
                // while the cancelled turn ran starts the next one.
                cx.drain().await;
                CommandEffect::persist(events).and_snapshot()
            }
            RunOutcome::Failed { error, recoverable } => {
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
                        context_tokens: state.context_tokens,
                    })
                    .await;
                parent
                    .deliver(AgentOutcome::Failed {
                        agent,
                        error,
                        recoverable,
                        // A run that failed inside the loop says nothing about
                        // whether the sandbox still exists.
                        terminal: false,
                    })
                    .await;
                // The partial turn was already journaled step by step, so the
                // failed run stays inspectable. The agent stays alive: a
                // failed turn is not a dead agent, and the next message
                // reuses it.
                CommandEffect::none()
            }
            RunOutcome::AlreadyReported => {
                // The failure was already delivered to the parent. Stay alive
                // so the next message can retry against the same transcript.
                CommandEffect::none()
            }
        }
    }

    /// What the tools that ended this run meant.
    ///
    /// A match on names, and nothing else. Each of these tools does exactly one
    /// thing, so there is no payload shape to disambiguate — which is the whole
    /// reason they are separate tools rather than one with a `kind` field.
    pub(super) fn interpret(calls: Vec<StoppedCall>) -> Conclusion {
        if calls.is_empty() {
            return Conclusion::Output(Value::Null);
        }
        // Several questions in one turn is ordinary: they are asked together
        // and answered together.
        if calls.iter().all(|c| c.tool == ASK_USER_TOOL) {
            return Conclusion::Ask(
                calls
                    .into_iter()
                    .map(|call| AskedQuestion {
                        tool_call_id: Some(call.tool_call_id),
                        question: call
                            .input
                            .get("question")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        // Read off the same input the transcript renders from,
                        // once, here — so the inbox and the transcript offer
                        // the identical set rather than each parsing the call
                        // for themselves.
                        choices: call
                            .input
                            .get("choices")
                            .and_then(Value::as_array)
                            .map(|cs| {
                                cs.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        multiple: call
                            .input
                            .get("multiple")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                    .collect(),
            );
        }
        if let [only] = calls.as_slice()
            && only.tool == SUBMIT_RESULT_TOOL
        {
            return Conclusion::Output(only.input.clone());
        }
        // Finishing *and* asking, or submitting twice: contradictory, and only
        // the model can resolve it. Every call gets an error result, so nothing
        // is left dangling, and the turn runs again.
        Conclusion::Contradiction(calls)
    }

    /// A step's turn ended with text instead of `submit_result`.
    ///
    /// That is legitimate exactly when something will wake this agent again: a
    /// queued message, an armed timer, or a subagent that still owes it a
    /// report. Otherwise nothing would ever start another turn and the step
    /// would sit "running" for ever, so the model is nudged — first with a
    /// plain message, then with `submit_result` forced, and only then is the
    /// step failed.
    ///
    /// All three facts are read off the shared state: the queue and the timers
    /// are in it, and the log carries every subagent lifecycle record the
    /// session wrote onto it. Nothing here asks another component anything.
    pub(super) async fn ended_without_result(
        &mut self,
        state: &AgentState,
        cx: &mut Cx<'_>,
        agent: uuid::Uuid,
        parent: Arc<dyn AgentOutcomeSink>,
    ) -> CommandEffect<AgentDomainEvent> {
        // The queue first: a subagent report that landed while the turn was
        // ending starts the next turn, and nothing needs classifying at all.
        if queued_turn(&state.inbox, &state.asks).is_some() {
            cx.drain().await;
            return CommandEffect::none();
        }
        if !state.timers.is_empty()
            || crate::agent_loop::carried_state::has_outstanding_children(state)
        {
            parent.deliver(AgentOutcome::Parked { agent }).await;
            let parked = AgentDomainEvent::Parked { at_ms: now_ms() };
            return CommandEffect::persist(vec![parked]).and_snapshot();
        }
        if state.nudges >= MAX_RESULT_NUDGES {
            parent
                .deliver(AgentOutcome::Failed {
                    agent,
                    error: format!(
                        "the step ended {} turns without calling `{SUBMIT_RESULT_TOOL}`, \
                         and nothing would wake it",
                        state.nudges + 1
                    ),
                    recoverable: false,
                    terminal: false,
                })
                .await;
            return CommandEffect::none();
        }
        // The second attempt names the tool in `tool_choice`, so the model can
        // emit nothing else. Not the first: a model that realises it is *not*
        // finished must still be able to go back to work, and a forcing would
        // forbid that.
        if state.nudges + 1 >= MAX_RESULT_NUDGES {
            cx.scratch.pending_tool_choice = Some(horsie_agentcore::ToolChoice::Required(
                SUBMIT_RESULT_TOOL.to_string(),
            ));
        }
        let nudge = AgentDomainEvent::Received {
            item: crate::agent_loop::Incoming::User {
                id: format!("nudge-result:{}", state.nudges),
                text: format!(
                    "Your turn ended without calling `{SUBMIT_RESULT_TOOL}`, and nothing will \
                     wake you — you have no armed timers and no delegated work still running. \
                     If the step's work is done, call `{SUBMIT_RESULT_TOOL}` now. If it is \
                     not, carry on working."
                ),
                // A nudge is the server talking to the model.
                artifacts: Vec::new(),
            },
            at_ms: now_ms(),
        };
        let nudged = AgentDomainEvent::Nudged { at_ms: now_ms() };
        // The drain told here finds the nudge folded into the queue and starts
        // the turn that answers it.
        cx.drain().await;
        CommandEffect::persist(vec![nudge, nudged])
    }

    /// The model called two turn-enders at once. Tell each call why, and run
    /// the turn again.
    ///
    /// Error results rather than silence: every `tool_use` needs a
    /// `tool_result` for the session to stay valid, and a call left
    /// dangling is indistinguishable later from a question still waiting on the
    /// user.
    pub(super) async fn correct_contradiction(
        &mut self,
        calls: Vec<StoppedCall>,
        state: &AgentState,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let named = calls
            .iter()
            .map(|c| c.tool.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let reason = format!(
            "You ended your turn with more than one finishing tool ({named}). Do one thing: \
             either ask the user, or submit this step's result."
        );
        let at_ms = now_ms();
        let mut events: Vec<AgentDomainEvent> = calls
            .iter()
            .map(|c| AgentDomainEvent::ToolComplete {
                tool_call_id: c.tool_call_id.clone(),
                output: reason.clone(),
                is_error: true,
                artifacts: Vec::new(),
                at_ms,
            })
            .collect();
        let nudged = AgentDomainEvent::Nudged { at_ms };
        events.push(nudged);
        let folded = Components::apply_all(state, &events);
        if folded.nudges > MAX_RESULT_NUDGES {
            return CommandEffect::persist(events);
        }
        let resume = AgentDomainEvent::Received {
            item: crate::agent_loop::Incoming::Continue {
                id: format!("contradiction:{}", folded.nudges),
                reason,
            },
            at_ms,
        };
        events.push(resume);
        // The continuation is in the queue once these persist; the drain
        // starts the turn that acts on it.
        cx.drain().await;
        CommandEffect::persist(events)
    }
}
