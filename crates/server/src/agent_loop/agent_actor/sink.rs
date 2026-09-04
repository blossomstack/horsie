//! The two sinks a run's events pass through on their way to the journal.
//!
//! [`PersistSink`] is where backpressure lives: it journals by `ask`ing the
//! actor and awaiting the durable write, so `emit().await` does not return
//! until the event is written and the LLM loop cannot outrun its own history.
//! Persistence still flows through the one mailbox, never the journal directly.
//!
//! [`CapturingSink`] wraps it and records what went past, which is how a failed
//! attempt can be asked the only question that decides whether to retry it: did
//! anything durable get written? [`coarse_event`] is the test it applies — the
//! same mapping `PersistSink` uses, not a proxy for it.

use super::*;
use async_trait::async_trait;
use horsie_actor::ActorRef;
use horsie_agentcore::{AgentEvent, EventSink, EventSinkError, LifecycleEvent};
use std::sync::{Arc, Mutex};

/// Captures coarse agent events while forwarding every event to the inner sink.
/// Used only inside [`run_with_retries`] to locate the handoff tool-call id;
/// persistence (with backpressure) happens in the inner [`PersistSink`].
pub(super) struct CapturingSink {
    inner: Arc<dyn EventSink>,
    captured: Mutex<Vec<AgentEvent>>,
}

impl CapturingSink {
    pub(super) fn new(inner: Arc<dyn EventSink>) -> Self {
        Self {
            inner,
            captured: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn take(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut self.captured.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[async_trait]
impl EventSink for CapturingSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(event.clone());
        }
        // Propagate the inner sink's outcome so a durability failure aborts the run.
        self.inner.emit(event).await
    }
}

/// Persists each coarse domain event by `ask`ing the agent actor and awaiting the
/// durable write before returning — this is what gives the agent loop end-to-end
/// backpressure. Persistence flows through the actor's mailbox
/// ([`AgentCommand::PersistProgress`]), never the journal directly.
///
/// This is the only sink. There used to be a second one forwarding every event
/// to a broadcast so a live stream could accumulate its own copy of the
/// transcript; readers now read the agent's state instead, so the copy — and
/// the ordering problem between it and the original — is gone.
///
/// `InputMessage` is intentionally NOT persisted here: the actor persists the input
/// itself when handling `Run`/`InjectToolResult`, so a turn-restarting retry that
/// re-emits the input can never double-persist it into two consecutive user
/// messages.
pub(super) struct PersistSink {
    pub(super) actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl EventSink for PersistSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let Some(coarse) = coarse_event(&event) {
            // Await the durable write and act on its outcome:
            // - Ok(Ok(()))  → journaled; proceed.
            // - Ok(Err(je)) → the journal write FAILED. Abort the run rather than
            //   continue on a history that was never recorded.
            // - Err(_)      → the actor has stopped (the run is being torn down), so
            //   there is nothing to persist to and nothing to wait for; drop quietly.
            match self
                .actor
                .ask(|ack| {
                    AgentCommand::Run(RunCommand::PersistProgress {
                        events: vec![coarse],
                        ack,
                    })
                })
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(je)) => {
                    return Err(EventSinkError(format!("journal write failed: {je}")));
                }
                Err(_actor_gone) => {}
            }
        }
        // Text chunks go through the same mailbox, unjournaled. `tell` rather
        // than `ask`: nothing durable happens, so there is nothing to wait for
        // — but it still travels the mailbox, which is what keeps a chunk from
        // overtaking the entry it precedes.
        if let AgentEvent::TextChunk(chunk) = &event {
            let _ = self
                .actor
                .tell(AgentCommand::Log(LogCommand::RecordDelta {
                    text: chunk.text.clone(),
                }))
                .await;
        }
        Ok(())
    }
}

/// Whether folding this event appends a log entry — i.e. consumes a `seq`.
///
/// Kept beside [`AgentState::apply_event`] deliberately: the two must agree, and
/// a variant added to one without the other would either strand deltas under an
/// entry that superseded them or clear them for an event that appended nothing.
pub(super) fn coarse_appends_an_entry(e: &AgentDomainEvent) -> bool {
    matches!(
        e,
        AgentDomainEvent::InputMessage { .. }
            | AgentDomainEvent::MessageComplete { .. }
            | AgentDomainEvent::MessageAborted { .. }
            | AgentDomainEvent::ToolComplete { .. }
            | AgentDomainEvent::HookRan { .. }
            | AgentDomainEvent::LifecycleRecorded { .. }
            | AgentDomainEvent::TaskListChanged { .. }
    )
}

/// Map a single streaming event to the coarse domain event that should be
/// persisted, or `None` for streaming noise and for `InputMessage` (see
/// [`PersistSink`]).
pub(super) fn coarse_event(e: &AgentEvent) -> Option<AgentDomainEvent> {
    match e {
        AgentEvent::MessageComplete(ev) => Some(AgentDomainEvent::MessageComplete {
            message: ev.message.clone(),
        }),
        AgentEvent::ToolComplete(ev) => Some(AgentDomainEvent::ToolComplete {
            tool_call_id: ev.tool_call_id.clone(),
            output: ev.output.clone(),
            is_error: ev.is_error,
            artifacts: ev.artifacts.clone(),
            original_output_bytes: ev.original_output_bytes,
            truncated_output_bytes: ev.truncated_output_bytes,
            spilled_output_bytes: ev.spilled_output_bytes,
            started_at_ms: ev.started_at_ms,
            // Carried on the streaming event, not re-read here: the in-memory
            // history already holds a message stamped with it.
            at_ms: ev.at_ms,
        }),
        AgentEvent::RunComplete(ev) => Some(AgentDomainEvent::RunComplete {
            usage: ev.usage.clone(),
            iterations: ev.iterations,
            context_tokens: ev.context_tokens,
            at_ms: ev.at_ms,
        }),
        AgentEvent::RunAborted(ev) => Some(AgentDomainEvent::RunAborted {
            usage: ev.usage.clone(),
            context_tokens: ev.context_tokens,
            at_ms: ev.at_ms,
        }),
        AgentEvent::Compacted(ev) => Some(AgentDomainEvent::Compacted {
            summary: ev.summary.clone(),
            carried_state: ev.carried_state.clone(),
            retained_from_message_id: ev.retained_from_message_id.clone(),
            trigger: ev.trigger.clone(),
            instructions: ev.instructions.clone(),
            tokens_before: ev.tokens_before,
            tokens_after: ev.tokens_after,
            at_ms: ev.at_ms,
        }),
        // A lifecycle entry rather than a `Compaction` one: nothing moved, so
        // nothing may look like a boundary. `prompt_messages` drops every
        // lifecycle body, so the notice answers the person and never reaches
        // the model — which is right, since the model was not asked anything.
        AgentEvent::CompactionSkipped(ev) => Some(AgentDomainEvent::LifecycleRecorded {
            event: LifecycleEvent::CompactionSkipped(ev.detail.clone()),
            at_ms: ev.at_ms,
        }),
        AgentEvent::InputMessage(_)
        | AgentEvent::MessageStart(_)
        | AgentEvent::MessageStop(_)
        | AgentEvent::TextBlockStart(_)
        | AgentEvent::TextChunk(_)
        | AgentEvent::ThinkingBlockStart(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ThinkingSignatureChunk(_)
        | AgentEvent::ToolCallStart(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ContentBlockStop(_)
        | AgentEvent::ToolExecuting(_) => None,
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
    use horsie_agentcore::AgentInput;
    use horsie_models::agent::{ArtifactKind, ArtifactRef, ImageArtifact, Usage};

    #[test]
    fn coarse_events_carry_the_tool_result_the_agent_recorded() {
        let artifact = ArtifactRef {
            id: "image-id".into(),
            media_type: "image/png".into(),
            kind: ArtifactKind::Image(ImageArtifact {
                width: Some(640),
                height: Some(480),
            }),
            byte_size: 12,
            filename: Some("page.png".into()),
        };
        let tool = coarse_event(&AgentEvent::ToolComplete(
            horsie_models::events::ToolCompleteEvent {
                message_id: "result:tc1".into(),
                tool_call_id: "tc1".into(),
                output: "ok".into(),
                is_error: false,
                artifacts: vec![artifact.clone()],
                original_output_bytes: 0,
                truncated_output_bytes: 0,
                spilled_output_bytes: 0,
                started_at_ms: 0,
                at_ms: 42,
            },
        ))
        .expect("ToolComplete is journaled");
        let AgentDomainEvent::ToolComplete {
            artifacts, at_ms, ..
        } = tool
        else {
            panic!("expected tool completion");
        };
        assert_eq!(at_ms, 42);
        assert_eq!(artifacts, vec![artifact]);

        let run = coarse_event(&AgentEvent::RunComplete(
            horsie_models::events::RunCompleteEvent {
                message_id: "run".into(),
                usage: Usage::without_cache(1, 1),
                iterations: 1,
                context_tokens: 1,
                at_ms: 99,
            },
        ))
        .expect("RunComplete is journaled");
        assert!(matches!(run, AgentDomainEvent::RunComplete { at_ms, .. } if at_ms == 99));
    }

    #[test]
    fn coarse_event_filters_streaming_noise_and_input() {
        use horsie_models::events::{InputMessageEvent, TextChunkEvent};
        // Streaming noise → None.
        assert!(
            coarse_event(&AgentEvent::TextChunk(TextChunkEvent {
                message_id: "m".into(),
                index: 0,
                text: "noise".into()
            }))
            .is_none()
        );
        // InputMessage is suppressed from the persistence stream (persisted by the
        // actor instead).
        assert!(
            coarse_event(&AgentEvent::InputMessage(InputMessageEvent {
                message_id: "m".into(),
                input: AgentInput::user_message("m", "hi")
            }))
            .is_none()
        );
    }
}
