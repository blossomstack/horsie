//! Where the prompt starts.
//!
//! A compaction never deletes: the log keeps everything it held and a boundary
//! entry records where the *prompt* now begins. Folding one is therefore
//! deterministic under replay, so a recovered agent prompts from exactly the
//! boundary the live one did — which is the whole reason the boundary is an
//! append rather than a truncation.
//!
//! The one subtlety lives in [`AgentState::resolve_boundary`]: the run that
//! produced the boundary was holding a `Vec<Message>` in *prompt* order, which
//! is not log order. Resolving the two is the fold's job because the fold is
//! the only thing holding the log.

use crate::agent_loop::prelude::*;
use horsie_actor::CommandEffect;
use horsie_agentcore::{AgentLogBody, CompactionEntry, ContentPart, Message, Role};
use horsie_models::now_ms;
/// The one message a compaction boundary shows the model.
///
/// Two labelled sections rather than one blob, because they have different
/// truth conditions. The summary is the model's own prose and may be wrong at
/// the edges. The carried state is exact and must survive verbatim — a
/// summariser that renders "task 3: in progress" as prose has destroyed the id
/// the agent needs to call `task_list` correctly.
///
/// A `User` message because that is the role every provider accepts as
/// context-setting; there is no wire on which a synthetic assistant turn with
/// no request behind it is safe.
#[must_use]
pub fn boundary_message(entry: &CompactionEntry, at_ms: u64) -> Message {
    let text = format!(
        "This session was compacted: earlier history is summarised below \
         rather than shown in full. The messages after this one are verbatim.\n\n\
         ## Summary of earlier work\n{}\n\n## Current state\n{}",
        entry.summary.trim(),
        entry.carried_state.trim(),
    );
    Message {
        // Derived from the boundary's own position, never generated, for the
        // reason `hook:{n}` is: replay must reproduce the id it produced live.
        id: format!("compaction:{}", entry.covers_through_seq),
        role: Role::User,
        parts: vec![ContentPart::Text(horsie_models::agent::TextPart { text })],
        created_at_ms: at_ms,
        started_at_ms: None,
    }
}

impl AgentState {
    /// What the model sees: the transcript, with every hook entry translated
    /// into the message it injects — most translate to nothing.
    ///
    /// The only way to obtain a `Vec<Message>` from state. `self.history`
    /// cannot be handed to a provider because the element types differ, so
    /// every kind of entry must state what, if anything, it shows the model;
    /// [`crate::agent_loop::shared::hook_translation::translate`] is where that is
    /// decided, in one exhaustive match, and any future non-model entry
    /// inherits the obligation.
    pub fn prompt_messages(&self) -> Vec<Message> {
        // Where the prompt starts. Everything below the newest boundary is
        // represented by that boundary's own message and nothing else — which
        // is the only thing compaction changes about this function.
        let boundary = self.last_boundary();
        let from_seq = boundary.map_or(0, |(_, e)| e.retained_from_seq);

        boundary
            .map(|(at_ms, e)| boundary_message(e, at_ms))
            .into_iter()
            .chain(
                self.log()
                    .iter()
                    .filter(|e| e.seq >= from_seq)
                    .filter_map(|e| match &e.body {
                        AgentLogBody::Llm(m) => Some(m.clone()),
                        AgentLogBody::Hook(h) => {
                            crate::agent_loop::shared::hook_translation::translate(h)
                        }
                        // Every lifecycle variant, present and future. This
                        // arm is the reason `Lifecycle` is one union rather
                        // than nine flattened ones: provider isolation cannot
                        // be forgotten for a variant added later.
                        AgentLogBody::Lifecycle(_) => None,
                        // A boundary reached here is never the newest one — the
                        // newest was lifted out above — so it is history. Its
                        // span is already folded into the summary of every
                        // boundary that followed it, and replaying it would put
                        // the same account in the prompt twice.
                        AgentLogBody::Compaction(_) => None,
                    }),
            )
            .collect()
    }

    /// The newest compaction boundary and the moment it was taken, if this
    /// agent has ever compacted.
    ///
    /// A reverse scan rather than a stored pointer: state is a serialization
    /// contract, and a cached index is a second thing that can disagree with
    /// the log after a partial fold. Boundaries are rare and the scan stops at
    /// the first one.
    #[must_use]
    pub fn last_boundary(&self) -> Option<(u64, &CompactionEntry)> {
        self.log().iter().rev().find_map(|e| match &e.body {
            AgentLogBody::Compaction(c) => Some((e.at_ms, c)),
            AgentLogBody::Llm(_) | AgentLogBody::Hook(_) | AgentLogBody::Lifecycle(_) => None,
        })
    }

    /// Turn "the retained window starts at this message" into log sequence
    /// numbers: `(covers_through_seq, retained_from_seq)`.
    ///
    /// Deterministic under replay because it is a search of an append-only log
    /// from a fold that has already applied everything before this event — the
    /// same log, the same id, the same answer.
    ///
    /// Two cases collapse to "retain nothing": no id at all (a summary-only
    /// compaction), and an id the log does not hold. The second should not
    /// happen — the run built its history from this log — but the honest
    /// failure is to show the model the summary alone rather than to guess a
    /// seq and silently resurrect or drop messages around it.
    #[must_use]
    pub(crate) fn resolve_boundary(&self, retained_from_message_id: Option<&str>) -> (u64, u64) {
        let retain_nothing = (self.tail_seq().unwrap_or(0), self.next_seq());
        let Some(id) = retained_from_message_id else {
            return retain_nothing;
        };
        let Some(idx) = self
            .log()
            .iter()
            .position(|e| e.body.id().is_some_and(|got| got == id))
        else {
            tracing::warn!(
                message_id = id,
                "a compaction named a message this log does not hold; \
                 retaining nothing"
            );
            return retain_nothing;
        };
        let retained_from_seq = self.log()[idx].seq;
        // The entry immediately before it in the log, read by position rather
        // than as `seq - 1`: the log is contiguous today, and this stays right
        // if it is ever front-trimmed.
        let covers_through_seq = idx
            .checked_sub(1)
            .map_or(retained_from_seq, |prev| self.log()[prev].seq);
        (covers_through_seq, retained_from_seq)
    }

    /// The seq of every compaction boundary, oldest first.
    ///
    /// These are the session ids: session N is the span
    /// `(previous boundary, this boundary]`, so the boundary that closes a
    /// session is what names it. A client seeking across compactions pages
    /// on these.
    #[must_use]
    pub fn boundary_seqs(&self) -> Vec<u64> {
        self.log()
            .iter()
            .filter(|e| matches!(e.body, AgentLogBody::Compaction(_)))
            .map(|e| e.seq)
            .collect()
    }
}

/// Folding old history behind a summary boundary.
///
/// The summarising call is a *special* run on purpose: no tools, no system
/// prompt, one request, text only — see [`summarise_step`]. Started by the
/// boundary, at a moment where every tool call is answered so nothing can be
/// cut across, and invisible to everything else: the next provider call simply
/// reads a shorter history.
pub(crate) struct CompactionStep;

impl CompactionStep {
    /// Whether the context has grown past the trigger — read off the budget
    /// the contexts publish. A fresh agent has no budget yet and never
    /// compacts before its first call, which is right: there is nothing to
    /// fold.
    pub(crate) fn due(cx: &CommandContext<'_>) -> bool {
        cx.step_run.execution.as_ref().is_some_and(|c| {
            c.budget
                .is_some_and(|b| cx.state.context_tokens() >= b.trigger_tokens())
        })
    }

    /// Take the summary on a spawned task.
    pub(crate) fn start(marker_seq: u64, job: CompactJob, cx: &mut CommandContext<'_>) {
        let Some(execution) = cx.step_run.execution.clone() else {
            return;
        };
        let cancel = cx.step_run.begin_compaction(marker_seq);
        // The history and the carried state are read here, at handling time: a
        // task-list change earlier in the same turn is already folded, so a
        // compaction between two calls carries it verbatim.
        let history = repair_unanswered_tool_calls(cx.state.prompt_messages());
        let carried_state =
            crate::agent_loop::shared::carried_state::render_carried_state(cx.state);
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            let (outcome, usage) = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                outcome = run_compaction(&job, &execution, history, carried_state, &cancel) => outcome,
            };
            let _ = self_ref
                .tell(AgentCommand::Compaction(CompactionCommand::Landed(
                    Box::new(CompactLanding {
                        marker_seq,
                        consumed: job.consumed,
                        usage,
                        outcome,
                    }),
                )))
                .await;
        });
    }
}

impl CompactionStep {
    pub(crate) async fn handle(
        cmd: CompactionCommand,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let CompactionCommand::Landed(landing) = cmd;
        let CompactLanding {
            marker_seq,
            consumed,
            usage,
            outcome,
        } = *landing;
        let marker_is_open = cx
            .state
            .open_step()
            .is_some_and(|(seq, kind)| seq == marker_seq && *kind == StepKind::Compaction);
        if !marker_is_open || !cx.step_run.finish_compaction(marker_seq) {
            tracing::warn!(
                marker_seq,
                "dropping a callback for a closed compaction step"
            );
            return CommandEffect::none();
        }
        let mut events = match outcome {
            CompactOutcome::Compacted(data) => vec![AgentDomainEvent::Compacted {
                summary: data.summary,
                carried_state: data.carried_state,
                retained_from_message_id: data.retained_from_message_id,
                trigger: data.trigger,
                instructions: data.instructions,
                tokens_before: data.tokens_before,
                tokens_after: data.tokens_after,
                usage,
                at_ms: now_ms(),
            }],
            CompactOutcome::Skipped {
                notice: true,
                context_tokens,
                retain_tokens,
            } => vec![AgentDomainEvent::LifecycleRecorded {
                event: horsie_agentcore::LifecycleEvent::CompactionSkipped(
                    horsie_models::agent::CompactionSkippedLifecycle {
                        context_tokens,
                        retain_tokens,
                    },
                ),
                at_ms: now_ms(),
            }],
            CompactOutcome::Skipped { notice: false, .. } => Vec::new(),
        };
        // The `/compact` that asked for this is crossed off now rather than
        // when it was taken: a crash in between replays it, and compacting
        // twice is cheaper than silently not compacting at all.
        if !consumed.is_empty() {
            events.push(AgentDomainEvent::Consumed {
                ids: consumed,
                at_ms: now_ms(),
            });
        }
        // Nothing is told that this happened. If a turn was owed a call, the
        // advance that follows this write makes it — over the compacted
        // history, which is the whole point.
        if events.is_empty() {
            cx.advance().await;
        }
        CommandEffect::persist(events)
    }
}

/// The compaction run itself, on its own task: decide the cut, fire the
/// hooks, take the summary, price the result. Answers what it produced and
/// what the summarising call spent.
async fn run_compaction(
    job: &CompactJob,
    execution: &ExecutionContext,
    history: Vec<Message>,
    carried_state: String,
    cancel: &tokio_util::sync::CancellationToken,
) -> (CompactOutcome, Option<horsie_agentcore::Usage>) {
    use horsie_models::agent::{CompactionTrigger, EmptyOutcome};
    let retain_tokens = execution.budget.map(|b| b.retain_tokens());
    let skipped = |notice: bool| CompactOutcome::Skipped {
        notice,
        context_tokens: job.tokens_before,
        retain_tokens,
    };
    let cut = horsie_agentcore::choose_cut(&history, retain_tokens.unwrap_or(0));
    if cut == 0 {
        // Nothing would be folded away. A typed `/compact` deserves to hear
        // that; the automatic check declining is routine and stays silent.
        return (skipped(job.manual), None);
    }
    let trigger_name = if job.manual { "manual" } else { "auto" };
    let records = execution
        .context_provider
        .compaction_hooks(horsie_models::runtime::ServerHookEvent::PreCompact(
            horsie_models::runtime::PreCompactInput {
                trigger: trigger_name.to_string(),
                instructions: job.instructions.clone(),
            },
        ))
        .await;
    if let Some(reason) = crate::agent_loop::shared::carried_state::precompact_refusal(&records) {
        tracing::info!(reason, "a PreCompact hook abandoned this compaction");
        return (skipped(false), None);
    }
    let (summary, usage) = match crate::agent_loop::shared::summarise::summarise_step(
        execution,
        &history,
        cut,
        job.instructions.as_deref(),
        cancel,
    )
    .await
    {
        Ok(taken) => taken,
        Err(e) => {
            tracing::warn!(error = %e, "a compaction failed; the turn continues uncompacted");
            return (skipped(false), None);
        }
    };
    let retained_from_message_id = history.get(cut).map(|m| m.id.clone());
    let boundary = Message {
        id: format!("compaction:{}", history.len()),
        role: Role::User,
        parts: vec![ContentPart::Text(horsie_models::agent::TextPart {
            text: horsie_agentcore::boundary_text(&summary, &carried_state),
        })],
        created_at_ms: now_ms(),
        started_at_ms: None,
    };
    let mut rewritten = vec![boundary];
    rewritten.extend_from_slice(&history[cut..]);
    let tokens_after = horsie_agentcore::approx_history_tokens(&rewritten);
    // Fire-and-forget: the boundary is about to exist, and nothing a
    // `PostCompact` hook says could change it.
    let _ = execution
        .context_provider
        .compaction_hooks(horsie_models::runtime::ServerHookEvent::PostCompact(
            horsie_models::runtime::PostCompactInput {
                trigger: trigger_name.to_string(),
                tokens_before: job.tokens_before,
                tokens_after,
            },
        ))
        .await;
    (
        CompactOutcome::Compacted(Box::new(CompactedData {
            summary,
            carried_state,
            retained_from_message_id,
            trigger: match job.manual {
                true => CompactionTrigger::Manual(EmptyOutcome {}),
                false => CompactionTrigger::Auto(EmptyOutcome {}),
            },
            instructions: job.instructions.clone(),
            tokens_before: job.tokens_before,
            tokens_after,
        })),
        Some(usage),
    )
}

impl CompactionStep {
    /// Where the prompt now starts, and the context size that leaves behind.
    // `if let` rather than a `match`, because this module owns exactly one
    // variant. Which one is decided in `AgentActor::apply_event`, so an event
    // added later fails to compile *there* — where it has to be classified —
    // rather than silently reaching the wrong fold here.
    pub(crate) fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        if let AgentDomainEvent::Compacted {
            summary,
            carried_state,
            retained_from_message_id,
            trigger,
            instructions,
            tokens_before,
            tokens_after,
            usage,
            at_ms,
        } = event
        {
            // The summarising call's cost, banked where every other cost is:
            // nothing routes usage through another component, and nothing but
            // the usage part knows how a cost is added.
            if let Some(usage) = &usage {
                state.bank_usage(usage);
            }
            let (covers_through_seq, retained_from_seq) =
                state.resolve_boundary(retained_from_message_id.as_deref());
            let entry = CompactionEntry {
                summary,
                carried_state,
                covers_through_seq,
                retained_from_seq,
                trigger,
                instructions,
                tokens_before,
                tokens_after,
            };
            // `context_tokens` is what the next auto-compaction check
            // reads, and the whole point of a compaction is that this
            // number just dropped. Leaving it at the pre-compaction size
            // would make the very next turn compact again immediately.
            state.context_is(tokens_after);
            state.push(at_ms, AgentLogBody::Compaction(entry));
        }
    }
}
