//! Timers, wired to the actor.
//!
//! The domain types — arming, remaining, the wake message — live in
//! [`crate::agent_loop::timers`], which stays pure. This is the other half: the
//! tool an agent calls, the commands that reach the mailbox, the fold, and the
//! re-arm that recovery owes.
//!
//! A timer firing does not run anything. It queues a wake, which waits in the
//! same place everything else addressed to this agent waits — so a timer firing
//! mid-run is harmless, and no flag has to remember anything.

use super::*;
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor};
use horsie_agentcore::ToolOutcome;
use horsie_agentcore::Toolbox;
use horsie_models::now_ms;
use serde_json::Value;
use std::sync::Arc;

/// Spawn a one-shot sleep that tells the actor `TimerFired` after `delay`. The
/// firing is journaled/handled in the actor; a stale fire (timer since cancelled)
/// is ignored there, so an un-cancellable sleep task is harmless.
pub(super) fn spawn_timer_sleep(
    self_ref: ActorRef<AgentCommand>,
    id: crate::agent_loop::timers::TimerId,
    delay: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = self_ref
            .tell(AgentCommand::Timer(TimerCommand::TimerFired { id }))
            .await;
    });
}

/// Wraps an agent's toolbox, adding the three timer control tools. They execute by
/// `ask`ing the owning [`AgentActor`] (never forwarded to the sandboxed runtime).
pub(super) struct TimerToolbox {
    pub(super) inner: Arc<dyn Toolbox>,
    pub(super) actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TimerToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(crate::agent_loop::timers::timer_tool_specs());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
        use crate::agent_loop::timers::{CancelSelector, TimerId, TimerKind};
        use horsie_agentcore::ToolCallError;
        match name {
            "set_timer" => {
                let kind = match input.get("kind").and_then(Value::as_str) {
                    Some("one_shot") => TimerKind::OneShot,
                    Some("recurring") => TimerKind::Recurring,
                    _ => {
                        return Err(ToolCallError::InvalidInput(
                            "set_timer.kind must be 'one_shot' or 'recurring'".to_string(),
                        ));
                    }
                };
                let Some(after_secs) = input
                    .get("after_secs")
                    .and_then(Value::as_u64)
                    .filter(|n| *n >= 1)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.after_secs must be an integer >= 1".to_string(),
                    ));
                };
                let label = input
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let Some(message) = input
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.message must be a non-empty string".to_string(),
                    ));
                };
                let id = self
                    .actor
                    .ask(|reply| {
                        AgentCommand::Timer(TimerCommand::ArmTimer {
                            label,
                            message,
                            kind,
                            after_secs,
                            reply,
                        })
                    })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                Ok(ToolOutcome::Result(serde_json::json!({ "timer_id": id.0 })))
            }
            "list_timers" => {
                let views = self
                    .actor
                    .ask(|reply| AgentCommand::Timer(TimerCommand::ListTimers { reply }))
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                serde_json::to_value(views)
                    .map(ToolOutcome::Result)
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))
            }
            "cancel_timer" => {
                let selector = if input.get("all").and_then(Value::as_bool) == Some(true) {
                    CancelSelector::All
                } else if let Some(id) = input.get("id").and_then(Value::as_str) {
                    CancelSelector::One(TimerId(id.to_string()))
                } else {
                    return Err(ToolCallError::InvalidInput(
                        "cancel_timer requires 'id' or 'all': true".to_string(),
                    ));
                };
                let ids = self
                    .actor
                    .ask(|reply| AgentCommand::Timer(TimerCommand::CancelTimer { selector, reply }))
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                let ids: Vec<String> = ids.into_iter().map(|i| i.0).collect();
                Ok(ToolOutcome::Result(serde_json::json!({ "cancelled": ids })))
            }
            _ => self.inner.execute(name, input, tool_call_id).await,
        }
    }
}

impl AgentActor {
    /// A timer's sleep elapsed. Re-arm a recurring timer, then queue the wake.
    ///
    /// Queued rather than run: a wake is one more thing addressed to this agent,
    /// and it waits in the same place everything else does. That is what makes a
    /// timer firing mid-run harmless — the run finishes, the boundary drains,
    /// and no flag has to remember anything.
    pub(super) async fn handle_timer_fired(
        &mut self,
        id: crate::agent_loop::timers::TimerId,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(record) = state.timers.iter().find(|t| t.id == id).cloned() else {
            // Cancelled or already removed — a stale sleep. Ignore.
            return CommandEffect::none();
        };
        let display_count = record.fire_count + 1;
        let now = now_ms();
        // Re-arm recurring; remove one-shot.
        let next_fire_at_unix_ms = match record.kind {
            crate::agent_loop::timers::TimerKind::Recurring => {
                let next = now.saturating_add(record.interval_secs.saturating_mul(1000));
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(record.interval_secs),
                );
                Some(next)
            }
            crate::agent_loop::timers::TimerKind::OneShot => None,
        };
        // Derived from the timer and its fire count, never generated: the fold
        // must reproduce the same id on replay, which a uuid could not.
        let received = AgentDomainEvent::Received {
            item: crate::agent_loop::Incoming::Timer {
                id: format!("{id}:{display_count}"),
                message: record.wake_message(display_count),
            },
            at_ms: now,
        };
        let fired = AgentDomainEvent::TimerFired {
            id,
            next_fire_at_unix_ms,
            at_ms: now,
        };
        let mut events = vec![fired, received];
        let folded = events
            .iter()
            .cloned()
            .fold(state.clone(), Self::apply_event);
        events.extend(self.try_drain(&folded, ctx).await);
        CommandEffect::persist(events)
    }
}

/// Timers this agent has armed against itself.
pub(super) struct Timers;

impl Timers {
    pub(super) async fn handle(
        actor: &mut AgentActor,
        state: &AgentState,
        cmd: TimerCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            TimerCommand::ArmTimer {
                label,
                message,
                kind,
                after_secs,
                reply,
            } => {
                let now = now_ms();
                let record = crate::agent_loop::timers::TimerRecord::arm(
                    label,
                    message,
                    kind,
                    std::time::Duration::from_secs(after_secs),
                    now,
                );
                let id = record.id.clone();
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(after_secs),
                );
                let _ = reply.send(id);
                CommandEffect::persist(vec![AgentDomainEvent::TimerArmed {
                    record,
                    at_ms: now_ms(),
                }])
            }
            TimerCommand::ListTimers { reply } => {
                let now = now_ms();
                let views = state.timers.iter().map(|t| t.view(now)).collect();
                let _ = reply.send(views);
                CommandEffect::none()
            }
            TimerCommand::CancelTimer { selector, reply } => {
                let ids: Vec<crate::agent_loop::timers::TimerId> = match selector {
                    crate::agent_loop::timers::CancelSelector::All => {
                        state.timers.iter().map(|t| t.id.clone()).collect()
                    }
                    crate::agent_loop::timers::CancelSelector::One(id) => {
                        if state.timers.iter().any(|t| t.id == id) {
                            vec![id]
                        } else {
                            vec![]
                        }
                    }
                };
                let _ = reply.send(ids.clone());
                if ids.is_empty() {
                    CommandEffect::none()
                } else {
                    CommandEffect::persist(vec![AgentDomainEvent::TimerCancelled {
                        ids,
                        at_ms: now_ms(),
                    }])
                }
            }
            TimerCommand::TimerFired { id } => actor.handle_timer_fired(id, state, ctx).await,
        }
    }
}

#[async_trait]
impl Component for Timers {
    /// Every timer that survived, re-armed with its remaining delay — firing
    /// immediately if it is already due. Whether the agent is parked or was
    /// mid-run, because a timer keeps its promise either way.
    async fn on_load(
        _actor: &mut AgentActor,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) {
        let now = now_ms();
        for t in &state.timers {
            spawn_timer_sleep(ctx.self_ref(), t.id.clone(), t.remaining(now));
        }
    }

    // The fallthrough is unreachable by construction: `AgentActor::apply_event`
    // routes every variant to exactly one module, so an event added later fails
    // to compile *there* — where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::TimerArmed { record, .. } => state.timers.push(record),
            AgentDomainEvent::TimerCancelled { ids, .. } => {
                state.timers.retain(|t| !ids.contains(&t.id));
            }
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms,
                ..
            } => match next_fire_at_unix_ms {
                Some(next) => {
                    if let Some(t) = state.timers.iter_mut().find(|t| t.id == id) {
                        t.fire_at_unix_ms = next;
                        t.fire_count += 1;
                    }
                }
                None => state.timers.retain(|t| t.id != id),
            },
            _ => {}
        }
    }
}
