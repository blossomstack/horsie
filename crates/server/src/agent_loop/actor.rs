//! The actor: one agent's shell.
//!
//! It routes every command to [`Components`], persists whatever they decided,
//! folds it, and keeps the plumbing they all rely on — the observer, the
//! revision counter, the snapshot cadence. It decides nothing itself, and it
//! does not know what components exist.
//!
//! Two things deliberately do not happen on this mailbox. No provider call and
//! no toolbox build: those run on a spawned task, so a thirty-second MCP
//! connect cannot block a cancel. And no decision about whether this agent
//! exists: residency belongs to whoever spawned it.

use crate::agent_loop::context::{AgentOutcome, AgentRuntimeContext};
use crate::agent_loop::prelude::*;
use async_trait::async_trait;
use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor, PersistenceId};
use std::sync::Arc;

/// Events an agent may journal between snapshots before the next one is taken.
///
/// An agent's state *is* its transcript, so a snapshot costs O(transcript) to
/// write — snapshotting every turn would be quadratic over a session. This
/// trades a bounded replay on recovery for a bounded write amplification.
const SNAPSHOT_EVERY_EVENTS: u64 = 200;

/// Whether folding this event appends a log entry — i.e. consumes a `seq`.
///
/// Kept beside the fold deliberately: the two must agree, and a variant added
/// to one without the other would either strand deltas under an entry that
/// superseded them or clear them for an event that appended nothing.
fn coarse_appends_an_entry(e: &AgentDomainEvent) -> bool {
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

/// Observer of an agent's durable history, notified once per event that is both
/// journaled and folded into state.
///
/// This is how a live stream learns what happened without reading the journal:
/// the actor is the only thing that touches its own log, and this is the seam
/// it publishes through. Implementations must not block — they run on the
/// actor's mailbox — and must treat delivery as best-effort.
pub trait AgentObserver: Send + Sync {
    /// `state` is the state *after* `event` was folded, so an observer that
    /// needs the resulting message can read the log's tail rather than
    /// re-deriving it from the event.
    fn publish(&self, event: &AgentDomainEvent, state: &AgentState);
}

/// An agent, modelled as an event-sourced actor over components.
///
/// Everything domain-shaped lives in a component; what stays here is the
/// routing and the plumbing every component relies on.
pub struct AgentActor {
    runtime: AgentRuntimeContext,
    params: AgentParams,
    /// The transient half of the state — see [`component::StepRun`].
    step_run: StepRun,
    /// Every component this agent runs, centralized in one registry the actor
    /// delegates to wholesale — it knows neither their types nor their number.
    components: Components,
    /// Where durable history is published, when anyone is listening. `None` for
    /// workflow agents, which have no live stream.
    observer: Option<Arc<dyn AgentObserver>>,
    /// Events journaled since a snapshot was last *requested*. Counting
    /// requests rather than confirmed writes means a failed snapshot simply
    /// waits another interval, which is the right instinct for an
    /// optimization: retrying hard against a failing journal helps nobody.
    events_since_snapshot: u64,
    /// This actor's own address, captured the first time it handles anything.
    /// The persist hook has no context of its own, and it is what tells the
    /// agent to reconsider once a write lands.
    self_ref: Option<horsie_actor::ActorRef<AgentCommand>>,
    /// A counter, bumped whenever this agent moves, for readers to wait on.
    /// Held behind an `Arc` because the *owner* is whoever outlives this actor
    /// — for a session agent that is the supervisor, so an idle offload does
    /// not disconnect a reader.
    revision: std::sync::Arc<tokio::sync::watch::Sender<crate::sessions::Revision>>,
}

impl AgentActor {
    pub fn new(ctx: AgentRuntimeContext, params: AgentParams) -> Self {
        let revision = ctx.revision.clone();
        let step_run = StepRun::new(ctx.ready);
        Self {
            runtime: ctx,
            params,
            step_run,
            components: Components::new(),
            observer: None,
            self_ref: None,
            events_since_snapshot: 0,
            revision,
        }
    }

    /// Same actor, publishing its durable history to `observer` — what a
    /// session agent needs and a workflow agent does not.
    pub fn with_observer(
        ctx: AgentRuntimeContext,
        params: AgentParams,
        observer: Arc<dyn AgentObserver>,
    ) -> Self {
        Self {
            observer: Some(observer),
            ..Self::new(ctx, params)
        }
    }

    /// The journal identity of an agent: kind `"agent"`, id = the agent's own
    /// [`AgentRuntimeContext::journal_id`]. Centralizes the kind so the
    /// workflow (e.g. sub session) and the actor agree.
    pub fn persistence_id_for(journal_id: uuid::Uuid) -> PersistenceId {
        PersistenceId::new("agent", journal_id.to_string())
    }

    /// Add the snapshot cadence to a component's effect: once enough events
    /// have accrued, the next persisting effect carries a snapshot along.
    ///
    /// Actor-level because it is storage policy, not a domain decision — a
    /// component still forces a snapshot itself where one is always right (a
    /// park, a seed), and the counter resets either way.
    fn with_snapshot_cadence(
        &mut self,
        effect: CommandEffect<AgentDomainEvent>,
    ) -> CommandEffect<AgentDomainEvent> {
        if effect.snapshots() {
            self.events_since_snapshot = 0;
            return effect;
        }
        if !effect.events().is_empty() && self.events_since_snapshot >= SNAPSHOT_EVERY_EVENTS {
            self.events_since_snapshot = 0;
            return effect.and_snapshot();
        }
        effect
    }
}

fn recovery_repairs(state: &AgentState) -> Vec<AgentDomainEvent> {
    let Some((_, kind)) = state.open_step() else {
        return Vec::new();
    };
    let at_ms = horsie_models::now_ms();
    match kind {
        StepKind::Initialize | StepKind::Connect => Vec::new(),
        StepKind::Agent => {
            if !state.open_step_has_response() {
                vec![
                    AgentDomainEvent::StepFailed {
                        reason: StepFailure::Interrupted,
                    },
                    AgentDomainEvent::RunCancelled { at_ms },
                    AgentDomainEvent::RunEnded {
                        reason: RunEnd::Interrupted,
                        at_ms,
                    },
                ]
            } else {
                missing_tool_results(&state.prompt_messages(), &parked_call_ids(state))
                    .into_iter()
                    .map(|message| AgentDomainEvent::InputMessage { message })
                    .collect()
            }
        }
        StepKind::Compaction => {
            let mut events = vec![AgentDomainEvent::StepFailed {
                reason: StepFailure::Interrupted,
            }];
            if let Some(crate::agent_loop::Offer::Compact { consumed, .. }) =
                crate::agent_loop::queued_offer(state.inbox(), state.asks())
            {
                events.push(AgentDomainEvent::Consumed {
                    ids: consumed,
                    at_ms,
                });
            }
            events
        }
        StepKind::SeedSummary { request_id } => {
            let Some(crate::agent_loop::Offer::Summary {
                consumed,
                sub_sessions,
            }) = crate::agent_loop::queued_offer(state.inbox(), state.asks())
            else {
                return vec![AgentDomainEvent::StepFailed {
                    reason: StepFailure::Interrupted,
                }];
            };
            vec![
                AgentDomainEvent::SeedSummaryTaken {
                    request_id: request_id.clone(),
                    sub_sessions,
                    result: Err("seed summary interrupted by recovery".to_string()),
                    usage: None,
                    at_ms,
                },
                AgentDomainEvent::Consumed {
                    ids: consumed,
                    at_ms,
                },
            ]
        }
        StepKind::StopHook => vec![
            AgentDomainEvent::StopHookCompleted {
                outcome: StopHookOutcome::Interrupted,
            },
            AgentDomainEvent::RunEnded {
                reason: RunEnd::Failed {
                    error: "stop hook interrupted by recovery".to_string(),
                    recoverable: true,
                    terminal: false,
                },
                at_ms,
            },
        ],
    }
}

#[async_trait]
impl EventSourcedActor for AgentActor {
    type Command = AgentCommand;
    type Event = AgentDomainEvent;
    type State = AgentState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.runtime.journal_id)
    }

    fn initial_state() -> AgentState {
        AgentState::default()
    }

    /// Fold one event into state — [`Components::apply`], the event-side twin
    /// of the registry's command routing, so live handling, replay and every
    /// component's own fold-forward agree.
    fn apply_event(state: AgentState, event: AgentDomainEvent) -> AgentState {
        Components::apply(state, event)
    }

    /// Hand the command to the component registry. The actor decides nothing
    /// here and does not know what components exist.
    async fn handle_command(
        &mut self,
        state: &AgentState,
        cmd: AgentCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        self.self_ref.get_or_insert_with(|| ctx.self_ref());
        let mut cx = Cx {
            state,
            step_run: &mut self.step_run,
            runtime: &self.runtime,
            params: &self.params,
            actor: ctx,
        };
        // The registry answers everything but the actor's own lifetime:
        // stopping is the one thing that is nobody's component.
        let effect = match self.components.handle(cmd, &mut cx).await {
            Some(effect) => effect,
            None => CommandEffect::stop(),
        };
        self.with_snapshot_cadence(effect)
    }

    /// Publish what just became durable. By the time this runs the events are
    /// written and folded, so `state` already contains what they appended.
    async fn on_events_persisted(&mut self, events: &[AgentDomainEvent], state: &AgentState) {
        self.events_since_snapshot = self
            .events_since_snapshot
            .saturating_add(events.len() as u64);
        // An entry supersedes every chunk that preceded it — the finished
        // message says everything they were building towards — so the deltas
        // are dropped the moment one lands.
        if events.iter().any(coarse_appends_an_entry) {
            self.step_run.deltas.clear();
        }
        self.revision.send_modify(|r| *r += 1);
        // A provider-backed step owns its cost. Publish the freshly folded
        // cumulative value only after that step's record is durable.
        if events.iter().any(|event| {
            matches!(
                event,
                AgentDomainEvent::MessageComplete { .. }
                    | AgentDomainEvent::Compacted { usage: Some(_), .. }
                    | AgentDomainEvent::SeedSummaryTaken { usage: Some(_), .. }
            )
        }) {
            self.runtime
                .parent
                .deliver(crate::agent_loop::AgentOutcome::UsageRecorded {
                    agent: self.runtime.journal_id,
                    usage_total: state.usage_total(),
                    context_tokens: state.context_tokens(),
                })
                .await;
        }
        for event in events {
            if let AgentDomainEvent::SeedSummaryTaken {
                sub_sessions,
                result,
                ..
            } = event
            {
                self.runtime
                    .parent
                    .deliver(AgentOutcome::SeedSummary {
                        agent: self.runtime.journal_id,
                        sub_sessions: sub_sessions.clone(),
                        result: result.clone(),
                    })
                    .await;
            }
        }
        // Terminal outcomes leave the actor only after their run boundary is
        // durable. Replaying a snapshot does not call this hook, and a parent
        // applies replacements by agent identity, so delivery is idempotent.
        for event in events {
            let AgentDomainEvent::RunEnded { reason, .. } = event else {
                continue;
            };
            let agent = self.runtime.journal_id;
            let outcome = match reason {
                RunEnd::Complete { output } => AgentOutcome::Concluded {
                    agent,
                    output: output.clone(),
                },
                RunEnd::AwaitingInput { asks } => AgentOutcome::Asked {
                    agent,
                    asks: asks.clone(),
                },
                RunEnd::Parked => AgentOutcome::Parked { agent },
                RunEnd::Cancelled => continue,
                RunEnd::Interrupted => AgentOutcome::Interrupted { agent },
                RunEnd::Failed {
                    error,
                    recoverable,
                    terminal,
                } => AgentOutcome::Failed {
                    agent,
                    error: error.clone(),
                    recoverable: *recoverable,
                    terminal: *terminal,
                },
            };
            self.runtime.parent.deliver(outcome).await;
        }
        // Whatever just became durable may have changed what this agent should
        // be doing — a queue item, a tool result, a boundary. Asking is cheap
        // and idempotent; the alternative is every writer remembering to.
        let terminal_failure = events.iter().any(|event| {
            matches!(
                event,
                AgentDomainEvent::RunEnded {
                    reason: RunEnd::Failed { .. },
                    ..
                }
            )
        });
        if !terminal_failure && let Some(self_ref) = &self.self_ref {
            let _ = self_ref
                .tell(AgentCommand::Core(CoreCommand::Advance))
                .await;
        }
        let Some(observer) = &self.observer else {
            return;
        };
        for event in events {
            observer.publish(event, state);
        }
    }

    /// Repair whatever the crash left half-done, before the first live
    /// command. Each component repairs its own; the actor only asks.
    async fn on_recovery_complete(
        &mut self,
        state: &AgentState,
        ctx: &mut ActorContext<AgentCommand>,
    ) {
        self.self_ref.get_or_insert_with(|| ctx.self_ref());
        // Announce where this incarnation starts. The channel outlives the
        // actor, so after an idle offload it still holds the position from
        // before — republishing costs nothing.
        self.revision.send_modify(|r| *r += 1);
        let mut cx = Cx {
            state,
            step_run: &mut self.step_run,
            runtime: &self.runtime,
            params: &self.params,
            actor: ctx,
        };
        let repair = recovery_repairs(state);
        if !repair.is_empty() {
            let (ack, _) = tokio::sync::oneshot::channel();
            cx.tell(AgentCommand::Run(RunCommand::PersistProgress {
                events: repair,
                ack: horsie_actor::ReplyTo::from_sender(ack),
            }))
            .await;
        }
        self.components.on_load(&mut cx).await;
        // Recovery is over and the repairs are queued behind this: the advance
        // lands after them and reads the state they leave.
        cx.advance().await;
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
    use horsie_actor::EventSourcedActor;
    use horsie_agentcore::{ContentPart, Message, Role};
    use horsie_models::agent::{ToolCallPart, Usage};

    fn fold(events: Vec<AgentDomainEvent>) -> AgentState {
        events
            .into_iter()
            .fold(AgentActor::initial_state(), AgentActor::apply_event)
    }

    fn assistant_with_calls(ids: &[&str]) -> Message {
        Message {
            id: "assistant".into(),
            role: Role::Assistant,
            parts: ids
                .iter()
                .map(|id| {
                    ContentPart::ToolCall(ToolCallPart {
                        id: (*id).into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "true"}),
                    })
                })
                .collect(),
            created_at_ms: 2,
            started_at_ms: None,
        }
    }

    #[test]
    fn recovery_fails_every_open_tool_call_without_dispatching() {
        let state = fold(vec![
            AgentDomainEvent::TurnBegan {
                consumed: vec!["u".into()],
                answered: Vec::new(),
                at_ms: 1,
            },
            AgentDomainEvent::StepStarted {
                kind: StepKind::Agent,
            },
            AgentDomainEvent::MessageComplete {
                message: assistant_with_calls(&["one", "two"]),
                usage: Usage::without_cache(10, 2),
            },
        ]);

        let repairs = recovery_repairs(&state);
        let repaired_ids: Vec<_> = repairs
            .iter()
            .filter_map(|event| match event {
                AgentDomainEvent::InputMessage { message } => message.parts.first(),
                _ => None,
            })
            .filter_map(|part| match part {
                ContentPart::ToolResult(result) => Some(result.tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(repaired_ids, ["one", "two"]);
        assert!(
            !repairs
                .iter()
                .any(|event| matches!(event, AgentDomainEvent::StepStarted { .. }))
        );
    }

    #[test]
    fn recovery_ends_an_unanswered_provider_step() {
        let state = fold(vec![AgentDomainEvent::StepStarted {
            kind: StepKind::Agent,
        }]);
        let repairs = recovery_repairs(&state);
        assert!(matches!(
            repairs.as_slice(),
            [
                AgentDomainEvent::StepFailed {
                    reason: StepFailure::Interrupted
                },
                AgentDomainEvent::RunCancelled { .. },
                AgentDomainEvent::RunEnded {
                    reason: RunEnd::Interrupted,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn old_marker_is_closed_by_run_end() {
        let state = fold(vec![
            AgentDomainEvent::StepStarted {
                kind: StepKind::Agent,
            },
            AgentDomainEvent::RunEnded {
                reason: RunEnd::Cancelled,
                at_ms: 2,
            },
            AgentDomainEvent::StepStarted {
                kind: StepKind::Agent,
            },
        ]);
        assert_eq!(state.open_step().map(|(seq, _)| seq), Some(2));
    }
}
