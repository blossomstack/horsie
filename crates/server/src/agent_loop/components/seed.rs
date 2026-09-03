//! Being a sub session, and being branched from.
//!
//! Branching changes nothing about the session being branched: the branch
//! point is a read, taken before anything is written so the number names the
//! moment the sub session was *asked for* rather than the moment its seed
//! happened to be built.
//!
//! Adopting a history is the other half, and it is one write. The message
//! rides along with the state rather than being enqueued separately for two
//! reasons, both learned the hard way: enqueued first, the sub session drains
//! and answers it before it has a history; enqueued after, a crash in between
//! leaves a seeded sub session with nothing to do.

use crate::agent_loop::prelude::*;
use horsie_actor::{CommandEffect, ReplyTo};
use horsie_agentcore::AgentLogBody;
use horsie_models::now_ms;

/// Being a sub session, and being branched from.
pub(crate) struct Seeding;

impl Seeding {
    pub(crate) async fn handle(
        &mut self,
        cmd: SeedCommand,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let state = cx.state;
        match cmd {
            SeedCommand::Snapshot { at_seq, reply } => {
                let _ = reply.send(Box::new(state.snapshot_at(at_seq)));
                CommandEffect::none()
            }
            SeedCommand::SeedFrom {
                state: seeded,
                seed,
                message,
                reply,
            } => {
                // Already seeded. Not an error: a process that died between
                // this write and the session journaling `SubSessionSeeded`
                // comes back and re-seeds, and the honest answer is that the
                // work is done. Saying otherwise would fail a sub session that
                // is perfectly fine.
                //
                // The inbox as well as the log, because only a summary seeds a
                // message: the other two modes leave the queued brief as the
                // whole of this write's trace, and a brief that is not a
                // person's message would not even log a `MessageQueued`.
                if !state.log().is_empty() || !state.pending_incoming().is_empty() {
                    let _ = reply.send(Ok(()));
                    return CommandEffect::none();
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let answer = match rx.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(format!("persist the sub session's history: {e}")),
                        Err(_) => Err("the sub session's history was never written".to_string()),
                    };
                    let _ = reply.send(answer);
                });
                let repairs = missing_tool_results(&seeded.prompt_messages(), &[]);
                let mut events = vec![AgentDomainEvent::Seeded {
                    state: seeded,
                    seed,
                }];
                events.extend(
                    repairs
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message }),
                );
                events.push(AgentDomainEvent::Received {
                    item: message,
                    at_ms: now_ms(),
                });
                CommandEffect::persist(events)
                    .and_ack(ReplyTo::from_sender(tx))
                    // A whole session in one event is exactly the case a
                    // snapshot exists for: without one, every later recovery
                    // replays it.
                    .and_snapshot()
            }
            SeedCommand::SummaryTaken {
                marker_seq,
                consumed,
                sub_sessions,
                result,
                usage,
            } => {
                let request_id = consumed.join(":");
                let marker_is_open = cx.state.open_step().is_some_and(|(seq, kind)| {
                    seq == marker_seq
                        && *kind
                            == (StepKind::SeedSummary {
                                request_id: request_id.clone(),
                            })
                });
                if !marker_is_open || !cx.step_run.finished(marker_seq) {
                    return CommandEffect::none();
                }
                let at_ms = now_ms();
                CommandEffect::persist(vec![
                    AgentDomainEvent::SeedSummaryTaken {
                        request_id,
                        sub_sessions,
                        result,
                        usage,
                        at_ms,
                    },
                    AgentDomainEvent::Consumed {
                        ids: consumed,
                        at_ms,
                    },
                ])
            }
        }
    }
}

impl Seeding {
    /// Take the summary the queued sub sessions are waiting on: a bare
    /// summarise run over the whole history at the branch point, sharing the
    /// compaction component's machinery.
    pub(crate) fn take_summary(
        &mut self,
        marker_seq: u64,
        consumed: Vec<String>,
        sub_sessions: Vec<uuid::Uuid>,
        cx: &mut Cx<'_>,
    ) {
        let Some(tctx) = cx.step_run.ctx.clone() else {
            return;
        };
        let cancel = cx.step_run.begin(StepPhase::SeedSummary, marker_seq);
        // The summary must describe the history at the branch point, read
        // before anything can append behind it.
        let history = repair_unanswered_tool_calls(cx.state.prompt_messages());
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                result = crate::agent_loop::shared::summarise::summarise_step(&tctx, &history, history.len(), None, &cancel)
                    => result,
            };
            let (result, usage) = match result {
                Ok((text, usage)) => (Ok(text), Some(usage)),
                Err(e) => {
                    tracing::warn!(error = %e, "summarising a session for a sub session failed");
                    (Err(e.to_string()), None)
                }
            };
            let _ = self_ref
                .tell(AgentCommand::Seed(SeedCommand::SummaryTaken {
                    marker_seq,
                    consumed,
                    sub_sessions,
                    result,
                    usage,
                }))
                .await;
        });
    }

    /// The history this agent adopted, and the seed appended after it.
    // `if let` rather than a `match`, because this module owns exactly one
    // variant. Which one is decided in `component::fold`, so an event added
    // later fails to compile *there* rather than silently reaching the wrong
    // fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        if let AgentDomainEvent::SeedSummaryTaken { usage, .. } = &event {
            // The summarising call's cost, banked where every other cost is.
            // Nothing else about this agent changed.
            if let Some(usage) = usage {
                state.bank_usage(usage);
            }
            return;
        }
        if let AgentDomainEvent::Seeded {
            state: seeded,
            seed,
        } = event
        {
            // Wholesale, because this is the agent's first event: anything
            // already here would be a bug rather than a history to merge.
            *state = *seeded;
            if let Some(seed) = seed {
                let at_ms = seed.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(*seed));
            }
        }
    }
}
