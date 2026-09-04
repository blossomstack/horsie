//! What a settled provider turn means for the surrounding agent run.

use super::*;

#[derive(Debug)]
enum Conclusion {
    /// The step's result, and the `submit_result` call that carried it —
    /// `None` when the run stopped with no calls at all.
    Output(Value, Option<String>),
    /// One or more questions, all parked on together.
    Ask(Vec<AskedQuestion>),
    /// Two turn-enders at once. The calls are named so each can be told why.
    Contradiction(Vec<StoppedCall>),
}

/// Interpret what ended the run — a tool that stopped it, or a plain-text
/// completion — and deliver the outcome to the parent. The turn's events
/// were already persisted step by step, so this only records the terminal
/// transition and lowers the gate.
pub(super) async fn conclude(
    report: RunReport,
    state: &AgentState,
    cx: &mut CommandContext<'_>,
) -> CommandEffect<AgentDomainEvent> {
    match report.outcome {
        RunOutcome::Completed { text } => {
            if cx.params.requires_result {
                return ended_without_result(state);
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
                if state.next_input().is_some() {
                    return CommandEffect::none();
                }
                if !state.timers().is_empty()
                    || crate::agent_loop::shared::carried_state::has_outstanding_children(state)
                {
                    let at_ms = now_ms();
                    return CommandEffect::persist(vec![
                        AgentDomainEvent::Parked { at_ms },
                        AgentDomainEvent::RunEnded {
                            reason: RunEnd::Parked,
                            at_ms,
                        },
                    ])
                    .and_snapshot();
                }
            }
            let _ = text;
            CommandEffect::none()
        }
        RunOutcome::Stopped { calls } => {
            match interpret(calls) {
                Conclusion::Output(output, submitted) => {
                    // Submitting says the work is done, which makes any
                    // armed timer moot: nothing is left for it to wake.
                    // Dropping them here rather than calling it a
                    // contradiction keeps one rule — the agent decides when
                    // it is finished — and avoids a failure mode the agent
                    // could not have been warned about at the tool
                    // boundary, where its own timers are invisible.
                    let mut events = Vec::new();
                    // The submitting call gets its result *journaled*: a
                    // dangling `tool_use` left behind would read as an
                    // open call to the next turn — and an actor that runs
                    // open calls would submit this result a second time.
                    if let Some(tool_call_id) = submitted {
                        events.push(AgentDomainEvent::ToolComplete {
                            tool_call_id,
                            output: "result submitted".to_string(),
                            is_error: false,
                            artifacts: Vec::new(),
                            at_ms: now_ms(),
                        });
                    }
                    if !state.timers().is_empty() {
                        events.push(AgentDomainEvent::TimerCancelled {
                            ids: state.timers().iter().map(|t| t.id.clone()).collect(),
                            at_ms: now_ms(),
                        });
                    }
                    let _ = output;
                    CommandEffect::persist(events)
                }
                Conclusion::Ask(asks) => {
                    // An ask is a turn boundary, but a parked agent only
                    // drains for a person changing their mind — the drain
                    // told here finds the asks folded in and holds
                    // anything queued behind them.
                    let at_ms = now_ms();
                    let recorded = AgentDomainEvent::AskRecorded {
                        asks: asks.clone(),
                        at_ms,
                    };
                    CommandEffect::persist(vec![
                        recorded,
                        AgentDomainEvent::RunEnded {
                            reason: RunEnd::AwaitingInput { asks },
                            at_ms,
                        },
                    ])
                    .and_snapshot()
                }
                Conclusion::Contradiction(calls) => correct_contradiction(calls, state),
            }
        }
        RunOutcome::Cancelled => {
            // The tokens were spent whatever became of the turn that spent
            // them; the caller banked them as `TurnAborted` in the same
            // batch, so the total read here includes them.
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
            if let Some(salvaged) = aborted_message(cx) {
                events.push(AgentDomainEvent::MessageAborted { message: salvaged });
            }
            let at_ms = now_ms();
            events.push(AgentDomainEvent::TurnCancelled { at_ms });
            events.push(AgentDomainEvent::RunEnded {
                reason: RunEnd::Cancelled,
                at_ms,
            });
            CommandEffect::persist(events).and_snapshot()
        }
        RunOutcome::Failed { error, recoverable } => CommandEffect::persist(vec![
            AgentDomainEvent::StepFailed {
                reason: StepFailure::Provider(error.clone()),
            },
            AgentDomainEvent::RunEnded {
                reason: RunEnd::Failed {
                    error,
                    recoverable,
                    terminal: false,
                },
                at_ms: now_ms(),
            },
        ]),
    }
}

/// What the tools that ended this run meant.
///
/// A match on names, and nothing else. Each of these tools does exactly one
/// thing, so there is no payload shape to disambiguate — which is the whole
/// reason they are separate tools rather than one with a `kind` field.
fn interpret(calls: Vec<StoppedCall>) -> Conclusion {
    if calls.is_empty() {
        return Conclusion::Output(Value::Null, None);
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
        return Conclusion::Output(only.input.clone(), Some(only.tool_call_id.clone()));
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
/// All three facts come from durable history and component state.
fn ended_without_result(state: &AgentState) -> CommandEffect<AgentDomainEvent> {
    // Pending input wins before classifying this ending.
    if state.next_input().is_some() {
        return CommandEffect::none();
    }
    if !state.timers().is_empty()
        || crate::agent_loop::shared::carried_state::has_outstanding_children(state)
    {
        let at_ms = now_ms();
        return CommandEffect::persist(vec![
            AgentDomainEvent::Parked { at_ms },
            AgentDomainEvent::RunEnded {
                reason: RunEnd::Parked,
                at_ms,
            },
        ])
        .and_snapshot();
    }
    if state.nudges() >= MAX_RESULT_NUDGES {
        let error = format!(
            "the step ended {} turns without calling `{SUBMIT_RESULT_TOOL}`, and nothing would wake it",
            state.nudges() + 1
        );
        return CommandEffect::persist(vec![AgentDomainEvent::RunEnded {
            reason: RunEnd::Failed {
                error,
                recoverable: false,
                terminal: false,
            },
            at_ms: now_ms(),
        }]);
    }
    // The next provider step derives whether `submit_result` must be
    // forced from the durable nudge count. A crash cannot lose that choice.
    let nudge = AgentDomainEvent::Received {
        item: crate::agent_loop::Incoming::User {
            id: format!("nudge-result:{}", state.nudges()),
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
    // Queued, not run: the nudge is something addressed to this agent, and
    // the advance that follows this write takes it like anything else.
    CommandEffect::persist(vec![nudge, nudged])
}

/// The model called two turn-enders at once. Tell each call why, and run
/// the turn again.
///
/// Error results rather than silence: every `tool_use` needs a
/// `tool_result` for the session to stay valid, and a call left
/// dangling is indistinguishable later from a question still waiting on the
/// user.
fn correct_contradiction(
    calls: Vec<StoppedCall>,
    state: &AgentState,
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
    let folded = RunLoop::apply_all(state, &events);
    if folded.nudges() > MAX_RESULT_NUDGES {
        return CommandEffect::persist(events);
    }
    let resume = AgentDomainEvent::Received {
        item: crate::agent_loop::Incoming::Continue {
            id: format!("contradiction:{}", folded.nudges()),
            reason,
        },
        at_ms,
    };
    events.push(resume);
    CommandEffect::persist(events)
}
