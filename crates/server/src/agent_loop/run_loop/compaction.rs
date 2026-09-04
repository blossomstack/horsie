//! Where the provider prompt starts.
//!
//! A compaction never deletes history. Its event names the first retained
//! message; transcript projection resolves that id to the dense visible cursor
//! and prompt construction replaces the covered prefix with one summary.

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
    fn prompt_from(transcript: &Transcript) -> Vec<Message> {
        let boundary = transcript.entries().iter().rev().find_map(|entry| {
            let AgentLogBody::Compaction(compaction) = &entry.body else {
                return None;
            };
            Some((entry.at_ms, compaction))
        });
        let from_seq = boundary.map_or(0, |(_, entry)| entry.retained_from_seq);

        boundary
            .map(|(at_ms, entry)| boundary_message(entry, at_ms))
            .into_iter()
            .chain(
                transcript
                    .entries()
                    .iter()
                    .filter(|entry| entry.seq >= from_seq)
                    .filter_map(|entry| match &entry.body {
                        AgentLogBody::Llm(message) => Some(message.clone()),
                        AgentLogBody::Hook(hook) => {
                            crate::agent_loop::shared::hook_translation::translate(hook)
                        }
                        AgentLogBody::Lifecycle(_) | AgentLogBody::Compaction(_) => None,
                    }),
            )
            .collect()
    }

    /// Build the current provider prompt.
    pub fn prompt_messages(&self) -> Vec<Message> {
        Self::prompt_from(&self.transcript())
    }

    /// Build the provider prompt sealed by a step marker.
    pub fn prompt_messages_through(&self, marker_seq: u64) -> Vec<Message> {
        let end = self
            .history()
            .partition_point(|entry| entry.seq <= marker_seq);
        Self::prompt_from(&project_transcript(&self.history()[..end]))
    }

    #[must_use]
    pub fn last_boundary(&self) -> Option<(u64, CompactionEntry)> {
        self.transcript()
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match &entry.body {
                AgentLogBody::Compaction(compaction) => Some((entry.at_ms, compaction.clone())),
                AgentLogBody::Llm(_) | AgentLogBody::Hook(_) | AgentLogBody::Lifecycle(_) => None,
            })
    }
    #[must_use]
    pub fn boundary_seqs(&self) -> Vec<u64> {
        self.transcript()
            .entries()
            .iter()
            .filter(|entry| matches!(entry.body, AgentLogBody::Compaction(_)))
            .map(|entry| entry.seq)
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
/// Whether the context has grown past the trigger — read off the budget
/// the contexts publish. A fresh agent has no budget yet and never
/// compacts before its first call, which is right: there is nothing to
/// fold.
pub(crate) fn due(cx: &CommandContext<'_>) -> bool {
    cx.state.prompt_changed_since_compaction()
        && cx.step_run.execution.as_ref().is_some_and(|context| {
            context
                .budget
                .is_some_and(|budget| cx.state.context_tokens() >= budget.trigger_tokens())
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
    let history = repair_unanswered_tool_calls(cx.state.prompt_messages_through(marker_seq));
    let carried_state = crate::agent_loop::shared::carried_state::render_carried_state(cx.state);
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

/// Fold compaction usage and the new context size. The transcript boundary
/// is projected directly from this event.
pub(crate) fn apply(state: &mut AgentState, event: AgentDomainEvent) {
    if let AgentDomainEvent::Compacted {
        usage,
        tokens_after,
        ..
    } = event
    {
        if let Some(usage) = &usage {
            state.bank_usage(usage);
        }
        state.context_is(tokens_after);
    }
}
