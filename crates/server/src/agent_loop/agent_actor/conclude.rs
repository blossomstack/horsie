//! What ended the run, and what that means.
//!
//! A turn ends one of four ways: a tool stopped it, the model wrote plain text,
//! it was cancelled, or it failed. Only the actor can tell a *park* from a
//! mistake, because only the actor knows what would wake it — a pending ask or
//! an armed timer means the silence is deliberate; nothing pending means the
//! turn owed a result and did not produce one.
//!
//! [`AgentActor::interpret`] is where the stopping tools are read, and it is
//! deliberately strict: two different finishing tools in one turn is a
//! contradiction, not a preference to resolve.

use super::*;
use crate::agent_loop::context::{AgentOutcome, AgentOutcomeSink, AskedQuestion};
use crate::sessions::ask_tool::ASK_USER_TOOL;
use crate::sessions::workflow::SUBMIT_RESULT_TOOL;
use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor};
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

impl AgentActor {
    /// Interpret what ended the run — a tool that stopped it, or a plain-text
    /// completion — and deliver the outcome to the parent. The session events
    /// were already persisted incrementally via
    /// [`AgentCommand::PersistProgress`], so this only records the terminal
    /// transition and decides the actor's lifecycle.
    pub(super) async fn handle_finished(
        &mut self,
        report: RunReport,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        // A report from a run that has already been superseded says nothing
        // about the run that is in flight now: clearing the handle on its word
        // would leave the live run unstoppable, and delivering its outcome
        // would tell the parent that a turn it never saw is over.
        if self.running.as_ref().map(|r| r.id) != Some(report.run_id) {
            tracing::warn!(
                run_id = report.run_id,
                current = ?self.running.as_ref().map(|r| r.id),
                "dropping the report of a superseded run"
            );
            return CommandEffect::none();
        }
        self.running = None;
        // Answered before any parent delivery below: a canceller is likely
        // blocking its own mailbox waiting on this, and those deliveries
        // `tell` into that same mailbox — replying first keeps the two from
        // deadlocking. The run task has already finished (this message is its
        // last act), so "it will write nothing more" is true now.
        for ack in self.cancel_acks.drain(..) {
            let _ = ack.send(());
        }
        let agent = self.ctx.journal_id;
        let parent = self.ctx.parent.clone();

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
                if self.params.requires_result {
                    return self.ended_without_result(state, ctx, agent, parent).await;
                }
                // An agent that owes its parent one report is not done while
                // work it delegated is still running: its conclusion would be
                // consumed now and the children's results would arrive at an
                // agent whose requester already moved on. The queue is checked
                // first — a child's report that landed mid-turn simply starts
                // the next turn — and otherwise the agent parks; the children
                // finishing is what wakes it, and its next conclusion is the
                // report.
                if self.params.park_on_outstanding_work {
                    let drained = self.try_drain(state, ctx).await;
                    if !drained.is_empty() {
                        return self.persist_maybe_snapshot(drained);
                    }
                    if !state.timers.is_empty()
                        || crate::agent_loop::carried_state::has_outstanding_children(state)
                    {
                        parent.deliver(AgentOutcome::Parked { agent }).await;
                        let parked = AgentDomainEvent::Parked { at_ms: now_ms() };
                        self.events_since_snapshot = 0;
                        return CommandEffect::persist(vec![parked]).and_snapshot();
                    }
                }
                parent
                    .deliver(AgentOutcome::Concluded {
                        agent,
                        output: Value::String(text),
                    })
                    .await;
                // Resident: the agent goes idle, it does not die. Its whole
                // transcript stays in memory for the next turn and for history
                // reads, and nothing has to replay a journal to answer either.
                //
                // A turn ending is a boundary, so whatever queued while it ran
                // starts the next one.
                let drained = self.try_drain(state, ctx).await;
                self.persist_maybe_snapshot(drained)
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
                        let mut folded = state.clone();
                        for e in &events {
                            folded = Self::apply_event(folded, e.clone());
                        }
                        events.extend(self.try_drain(&folded, ctx).await);
                        self.persist_maybe_snapshot(events)
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
                        // Recorded before the drain is decided, and the drain
                        // is asked against the folded result: an ask is a turn
                        // boundary, but a parked agent only drains for a
                        // person changing their mind — a report queued behind
                        // the question waits for it to be answered.
                        let recorded = AgentDomainEvent::AskRecorded {
                            asks,
                            at_ms: now_ms(),
                        };
                        let folded = Self::apply_event(state.clone(), recorded.clone());
                        let mut events = vec![recorded];
                        events.extend(self.try_drain(&folded, ctx).await);
                        // Snapshot to compact the incrementally-persisted log.
                        // Unconditional now that no cursor is a journal
                        // position: history and streams read state, so
                        // compaction is invisible.
                        self.events_since_snapshot = 0;
                        CommandEffect::persist(events).and_snapshot()
                    }
                    Conclusion::Contradiction(calls) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                                context_tokens: state.context_tokens,
                            })
                            .await;
                        self.correct_contradiction(calls, state, ctx).await
                    }
                }
            }
            RunOutcome::Cancelled => {
                // The tokens were spent whatever became of the turn that spent
                // them, and `RunAborted` has already landed — the sink awaits
                // each coarse write before `RunFinished` is told — so the total
                // read here is the one that includes them.
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
                // later turn. The journal is then a faithful record of what
                // the model was shown, and a mid-history dangle can no longer
                // accumulate.
                let mut events: Vec<AgentDomainEvent> =
                    missing_tool_results(&state.prompt_messages(), &parked_call_ids(state))
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message })
                        .collect();
                // Whatever the model had already written is the only copy
                // there is: deltas are unjournaled by design, and the boundary
                // entry the stop is about to append clears them. Twenty-two
                // minutes of generation used to end here, with the transcript
                // showing no sign a turn had run at all.
                //
                // After the synthetic results, not before: a cancelled call's
                // result belongs directly under the message that made it, and
                // this text is a later message than that one.
                if let Some(salvaged) = self.aborted_message() {
                    events.push(AgentDomainEvent::MessageAborted { message: salvaged });
                }
                events.push(AgentDomainEvent::RunCancelled { at_ms: now_ms() });
                // Snapshot to compact the incrementally-persisted log on
                // cancel.
                self.events_since_snapshot = 0;
                // A stop cancels the turn, not the promise: anything queued
                // while the cancelled turn ran starts the next one.
                let folded = events
                    .iter()
                    .cloned()
                    .fold(state.clone(), Self::apply_event);
                events.extend(self.try_drain(&folded, ctx).await);
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
                // The partial session was already journaled incrementally, so
                // the failed session stays inspectable. The agent stays alive:
                // a failed turn is not a dead agent, and the next message
                // reuses it.
                CommandEffect::none()
            }
            RunOutcome::AlreadyReported => {
                // Context preparation failed before the loop began; the
                // failure was already delivered to the parent. Stay alive so
                // the next message can retry against the same in-memory
                // transcript.
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
    /// All three facts are this actor's own: the queue and the timers are in
    /// its state, and its log carries every subagent lifecycle record the
    /// session wrote onto it. Nothing here asks the session anything.
    pub(super) async fn ended_without_result(
        &mut self,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
        agent: uuid::Uuid,
        parent: Arc<dyn AgentOutcomeSink>,
    ) -> CommandEffect<AgentDomainEvent> {
        // The queue first: a subagent report that landed while the turn was
        // ending starts the next turn, and nothing needs classifying at all.
        let drained = self.try_drain(state, ctx).await;
        if !drained.is_empty() {
            return self.persist_maybe_snapshot(drained);
        }
        if !state.timers.is_empty()
            || crate::agent_loop::carried_state::has_outstanding_children(state)
        {
            parent.deliver(AgentOutcome::Parked { agent }).await;
            let parked = AgentDomainEvent::Parked { at_ms: now_ms() };
            self.events_since_snapshot = 0;
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
            self.pending_tool_choice = Some(horsie_agentcore::ToolChoice::Required(
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
            },
            at_ms: now_ms(),
        };
        let nudged = AgentDomainEvent::Nudged { at_ms: now_ms() };
        let mut folded = Self::apply_event(state.clone(), nudge.clone());
        folded = Self::apply_event(folded, nudged.clone());
        let mut events = vec![nudge, nudged];
        events.extend(self.try_drain(&folded, ctx).await);
        CommandEffect::persist(events)
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
        ctx: &ActorContext<AgentCommand>,
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
                at_ms,
            })
            .collect();
        let nudged = AgentDomainEvent::Nudged { at_ms };
        events.push(nudged.clone());
        let mut folded = state.clone();
        for e in &events {
            folded = Self::apply_event(folded, e.clone());
        }
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
        folded = Self::apply_event(folded, resume.clone());
        events.push(resume);
        events.extend(self.try_drain(&folded, ctx).await);
        CommandEffect::persist(events)
    }
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
    fn stopped(calls: &[(&str, serde_json::Value)]) -> Vec<StoppedCall> {
        calls
            .iter()
            .enumerate()
            .map(|(i, (tool, input))| StoppedCall {
                tool: (*tool).to_string(),
                tool_call_id: format!("toolu_{i}"),
                input: input.clone(),
            })
            .collect()
    }

    /// The whole of the interpretation: a match on the tool's name. There is no
    /// payload shape to disambiguate, which is why these are separate tools
    /// rather than one with a `kind` field.
    #[test]
    fn a_submit_is_the_steps_result_verbatim() {
        let calls = stopped(&[(
            SUBMIT_RESULT_TOOL,
            serde_json::json!({"outcome": "p0", "description": "did it"}),
        )]);
        match AgentActor::interpret(calls) {
            Conclusion::Output(v) => {
                assert_eq!(v["outcome"], "p0");
                assert_eq!(v["description"], "did it");
            }
            other => panic!("expected the step's result, got {other:?}"),
        }
    }

    #[test]
    fn one_ask_parks_on_one_question() {
        let calls = stopped(&[(ASK_USER_TOOL, serde_json::json!({"question": "which?"}))]);
        match AgentActor::interpret(calls) {
            Conclusion::Ask(asks) => {
                assert_eq!(asks.len(), 1);
                assert_eq!(asks[0].question, "which?");
                assert_eq!(asks[0].tool_call_id.as_deref(), Some("toolu_0"));
            }
            other => panic!("expected an ask, got {other:?}"),
        }
    }

    /// Several questions in one turn are ordinary — they are asked together and
    /// answered together — so all of them are parked on.
    #[test]
    fn several_asks_park_on_all_of_them() {
        let calls = stopped(&[
            (ASK_USER_TOOL, serde_json::json!({"question": "first?"})),
            (ASK_USER_TOOL, serde_json::json!({"question": "second?"})),
        ]);
        match AgentActor::interpret(calls) {
            Conclusion::Ask(asks) => {
                let questions: Vec<&str> = asks.iter().map(|a| a.question.as_str()).collect();
                assert_eq!(questions, vec!["first?", "second?"]);
            }
            other => panic!("expected two asks, got {other:?}"),
        }
    }

    /// Finishing and asking at once has no honest reading, and neither does
    /// submitting twice. Only the model can resolve it, so every call is told
    /// why and the turn runs again.
    #[test]
    fn two_different_finishing_tools_are_a_contradiction() {
        let calls = stopped(&[
            (SUBMIT_RESULT_TOOL, serde_json::json!({"outcome": "p0"})),
            (ASK_USER_TOOL, serde_json::json!({"question": "or?"})),
        ]);
        assert!(matches!(
            AgentActor::interpret(calls),
            Conclusion::Contradiction(c) if c.len() == 2
        ));
    }

    #[test]
    fn submitting_twice_is_a_contradiction() {
        let calls = stopped(&[
            (SUBMIT_RESULT_TOOL, serde_json::json!({"outcome": "p0"})),
            (SUBMIT_RESULT_TOOL, serde_json::json!({"outcome": "p2"})),
        ]);
        assert!(matches!(
            AgentActor::interpret(calls),
            Conclusion::Contradiction(_)
        ));
    }
}

/// The run-id fence: a report can only speak for the run it came from.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod fence_tests {
    use super::*;
    use crate::agent_loop::AgentRunDef;
    use crate::agent_loop::agent_actor::testing::*;
    use crate::agent_loop::context::AgentOutcome;
    use crate::agent_loop::context::{ContextError, ContextProvider, Contexts};
    use horsie_actor::{ActorSystem, InMemoryJournal};
    use horsie_agentcore::{AgentLogBody, ContentPart, LifecycleEvent, Role};

    struct HangingProvider;
    #[async_trait]
    impl ContextProvider for HangingProvider {
        async fn provide(&self) -> Result<Contexts, ContextError> {
            std::future::pending().await
        }
    }

    /// A run that was superseded can still be unwinding, and its report must
    /// not be mistaken for the live run's. Taking its word for it would clear
    /// the live run's handle — leaving a turn nobody can stop and a parent
    /// told that a turn it never saw is over.
    #[tokio::test]
    async fn a_report_from_a_superseded_run_is_ignored() {
        let (tx, mut outcomes) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingProvider),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(OutcomeChannel(tx)),
            journal_id: uuid::Uuid::new_v4(),
            ready: true,
        };
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        });
        params.interactive = true;
        let journal = Arc::new(InMemoryJournal::new());
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );

        // Run 0 starts and hangs in `provide`, so it is genuinely in flight.
        agent
            .tell(AgentCommand::Queue(QueueCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: "m5".into(),
                    text: "first".into(),
                },
                ack: None,
            }))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // A report from some earlier run arrives late.
        agent
            .tell(AgentCommand::Run(RunCommand::RunFinished(Box::new(
                RunReport {
                    run_id: 99,
                    outcome: RunOutcome::Completed {
                        text: "from a run that is over".into(),
                    },
                    seed_summary: None,
                },
            ))))
            .await
            .unwrap();

        // Run 0 is still in flight, so a second turn is refused — the fence
        // held. Without it, `running` would have been cleared and this would
        // start a second background loop against the same journal.
        agent
            .tell(AgentCommand::Queue(QueueCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: "m6".into(),
                    text: "second".into(),
                },
                ack: None,
            }))
            .await
            .unwrap();

        let (reply, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Read(ReadCommand::PageLog {
                anchor: crate::agent_loop::Anchor::Tail,
                max: 50,
                filter: crate::agent_loop::LogFilter::everything(),
                reply: ReplyTo::from_sender(reply),
            }))
            .await
            .unwrap();
        let page = rx.await.unwrap();
        // The second message is *queued* — that much is its whole point — but
        // no second turn took it: one `TurnBegan`, one input message. Without
        // the fence, the stale report would have cleared `running` and the
        // second message would have started a run against the same journal.
        let began = page
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.body,
                    AgentLogBody::Lifecycle(LifecycleEvent::TurnBegan(_))
                )
            })
            .count();
        assert_eq!(
            began, 1,
            "the refused turn must not begin: {:?}",
            page.entries
        );
        assert!(
            outcomes
                .try_recv()
                .is_ok_and(|o| matches!(o, AgentOutcome::Started { .. })),
            "the first turn's own start, and nothing from the superseded run"
        );
        assert!(
            outcomes.try_recv().is_err(),
            "a superseded run's outcome must not reach the parent"
        );
    }

    /// Stopping a turn keeps what it had already written.
    ///
    /// Streamed text lives only in the deltas — unjournaled by design, since a
    /// finished message supersedes them within the second — and a cancelled
    /// call never produces that finished message. The boundary entry the stop
    /// appends then cleared them, so twenty-two minutes of generation ended
    /// with a transcript showing no sign a turn had run.
    #[tokio::test]
    async fn a_stopped_turn_keeps_the_text_it_had_already_written() {
        let (tx, _outcomes) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingProvider),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(OutcomeChannel(tx)),
            journal_id: uuid::Uuid::new_v4(),
            ready: true,
        };
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        });
        params.interactive = true;
        let journal = Arc::new(InMemoryJournal::new());
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );

        agent
            .tell(AgentCommand::Queue(QueueCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: "m1".into(),
                    text: "write me an essay".into(),
                },
                ack: None,
            }))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The same road a streamed chunk takes: the sink tells the actor.
        for chunk in ["Once upon ", "a time"] {
            agent
                .tell(AgentCommand::Log(LogCommand::RecordDelta {
                    text: chunk.to_string(),
                }))
                .await
                .unwrap();
        }

        let (ack, cancelled) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Run(RunCommand::Cancel {
                ack: Some(ReplyTo::from_sender(ack)),
            }))
            .await
            .unwrap();
        cancelled.await.unwrap();

        let (reply, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Read(ReadCommand::PageLog {
                anchor: crate::agent_loop::Anchor::Tail,
                max: 50,
                filter: crate::agent_loop::LogFilter::everything(),
                reply: ReplyTo::from_sender(reply),
            }))
            .await
            .unwrap();
        let page = rx.await.unwrap();
        let kept: Vec<String> = page
            .entries
            .iter()
            .filter_map(|e| {
                let AgentLogBody::Llm(m) = &e.body else {
                    return None;
                };
                if m.role != Role::Assistant {
                    return None;
                }
                let text: String = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => Some(t.text.clone()),
                        ContentPart::Thinking(_)
                        | ContentPart::ToolCall(_)
                        | ContentPart::ToolResult(_)
                        | ContentPart::SubAgentResult(_) => None,
                    })
                    .collect();
                (!text.is_empty()).then_some(text)
            })
            .collect();
        assert_eq!(
            kept,
            vec!["Once upon a time"],
            "the stopped turn's generation is gone: {:?}",
            page.entries
        );
    }
}
