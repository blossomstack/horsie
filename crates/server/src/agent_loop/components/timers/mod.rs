//! The timers component.
//!
//! The domain types — arming, remaining, the wake message — live in
//! [`crate::agent_loop::timers`], which stays pure. This is the other half:
//! the inline tool executor the turn routes to, the fold, the re-arm recovery
//! owes, and the one command a sleep elapsing sends.
//!
//! A timer firing does not run anything. It queues a wake, which waits in the
//! same place everything else addressed to this agent waits — a timer firing
//! mid-run is harmless, and no flag has to remember anything.

pub mod domain;

use crate::agent_loop::prelude::*;
use async_trait::async_trait;
use horsie_actor::{ActorRef, CommandEffect};
use horsie_models::now_ms;
use serde_json::Value;

/// Timers this agent has armed against itself.
///
/// Durable so they re-arm on recovery: a timer is a promise the agent made to
/// itself, and a crash must not silently drop it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TimerState {
    armed: Vec<crate::agent_loop::components::timers::domain::TimerRecord>,
}

impl TimerState {
    pub(crate) fn records(&self) -> &[crate::agent_loop::components::timers::domain::TimerRecord] {
        &self.armed
    }

    fn arm(&mut self, record: crate::agent_loop::components::timers::domain::TimerRecord) {
        self.armed.push(record);
    }

    fn cancel(&mut self, ids: &[crate::agent_loop::components::timers::domain::TimerId]) {
        self.armed.retain(|t| !ids.contains(&t.id));
    }

    fn fired(&mut self, id: &crate::agent_loop::components::timers::domain::TimerId, next: Option<u64>) {
        match next {
            Some(next) => {
                if let Some(t) = self.armed.iter_mut().find(|t| t.id == *id) {
                    t.fire_at_unix_ms = next;
                    t.fire_count += 1;
                }
            }
            None => self.armed.retain(|t| t.id != *id),
        }
    }
}

impl PartState for TimerState {
    /// Nothing: a timer belongs to the agent that armed it, and a sub session
    /// waking on one nobody set for it would be a surprise.
    fn carried(&self) -> Option<Self> {
        None
    }
}

/// Spawn a one-shot sleep that tells the actor `TimerFired` after `delay`. The
/// firing is journaled/handled on the mailbox; a stale fire (timer since
/// cancelled) is ignored there, so an un-cancellable sleep task is harmless.
pub(crate) fn spawn_timer_sleep(
    self_ref: ActorRef<AgentCommand>,
    id: crate::agent_loop::components::timers::domain::TimerId,
    delay: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = self_ref
            .tell(AgentCommand::Timer(TimerCommand::TimerFired { id }))
            .await;
    });
}

/// Execute one timer tool: the value it answers and the events that record it.
fn execute_timer_tool(
    folded: &AgentState,
    name: &str,
    input: &Value,
    self_ref: ActorRef<AgentCommand>,
) -> Result<(Value, Vec<AgentDomainEvent>), horsie_agentcore::ToolCallError> {
    use crate::agent_loop::components::timers::domain::{TimerId, TimerKind, TimerRecord};
    use horsie_agentcore::ToolCallError;
    let invalid = |m: &str| Err(ToolCallError::InvalidInput(m.to_string()));
    match name {
        "set_timer" => {
            let kind = match input.get("kind").and_then(Value::as_str) {
                Some("one_shot") => TimerKind::OneShot,
                Some("recurring") => TimerKind::Recurring,
                _ => return invalid("set_timer.kind must be 'one_shot' or 'recurring'"),
            };
            let Some(after_secs) = input
                .get("after_secs")
                .and_then(Value::as_u64)
                .filter(|n| *n >= 1)
            else {
                return invalid("set_timer.after_secs must be an integer >= 1");
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
                return invalid("set_timer.message must be a non-empty string");
            };
            let now = now_ms();
            let delay = std::time::Duration::from_secs(after_secs);
            let record = TimerRecord::arm(label, message, kind, delay, now);
            let id = record.id.clone();
            spawn_timer_sleep(self_ref, id.clone(), delay);
            Ok((
                serde_json::json!({ "timer_id": id.0 }),
                vec![AgentDomainEvent::TimerArmed { record, at_ms: now }],
            ))
        }
        "list_timers" => {
            let now = now_ms();
            let views: Vec<_> = folded.timers().iter().map(|t| t.view(now)).collect();
            serde_json::to_value(views)
                .map(|v| (v, Vec::new()))
                .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))
        }
        "cancel_timer" => {
            let ids: Vec<TimerId> = if input.get("all").and_then(Value::as_bool) == Some(true) {
                folded.timers().iter().map(|t| t.id.clone()).collect()
            } else if let Some(id) = input.get("id").and_then(Value::as_str) {
                let id = TimerId(id.to_string());
                folded
                    .timers()
                    .iter()
                    .any(|t| t.id == id)
                    .then_some(id)
                    .into_iter()
                    .collect()
            } else {
                return invalid("cancel_timer requires 'id' or 'all': true");
            };
            let named: Vec<String> = ids.iter().map(|i| i.0.clone()).collect();
            let events = match ids.is_empty() {
                true => Vec::new(),
                false => vec![AgentDomainEvent::TimerCancelled {
                    ids,
                    at_ms: now_ms(),
                }],
            };
            Ok((serde_json::json!({ "cancelled": named }), events))
        }
        other => invalid(&format!("no tool named '{other}'")),
    }
}

/// Timers this agent has armed against itself.
pub(crate) struct Timers;

#[async_trait]
impl Component for Timers {
    type Command = TimerCommand;

    /// A timer's sleep elapsed. Re-arm a recurring timer, then queue the wake.
    ///
    /// Queued rather than run: a wake is one more thing addressed to this
    /// agent, and the `Drain` told here finds it in the same durable queue
    /// everything else waits in — a timer firing mid-run is harmless.
    async fn handle(
        &mut self,
        cmd: TimerCommand,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let id = match cmd {
            TimerCommand::ToolCall(call) => {
                return answer_tool_call(call, cx, execute_timer_tool).await;
            }
            TimerCommand::TimerFired { id } => id,
        };
        let Some(record) = cx.state.timers().iter().find(|t| t.id == id).cloned() else {
            // Cancelled or already removed — a stale sleep. Ignore.
            return CommandEffect::none();
        };
        let display_count = record.fire_count + 1;
        let now = now_ms();
        // Re-arm recurring; remove one-shot.
        let next_fire_at_unix_ms = match record.kind {
            crate::agent_loop::components::timers::domain::TimerKind::Recurring => {
                spawn_timer_sleep(
                    cx.actor.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(record.interval_secs),
                );
                Some(now.saturating_add(record.interval_secs.saturating_mul(1000)))
            }
            crate::agent_loop::components::timers::domain::TimerKind::OneShot => None,
        };
        // The wake id is derived from the timer and its fire count, never
        // generated: the fold must reproduce the same id on replay.
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
        CommandEffect::persist(vec![fired, received])
    }

    // The fallthrough is unreachable by construction: `component::fold` routes
    // every variant to exactly one component, so an event added later fails to
    // compile *there* rather than silently reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::TimerArmed { record, .. } => {
                if let Some(part) = state.part_mut::<TimerState>() {
                    part.arm(record);
                }
            }
            AgentDomainEvent::TimerCancelled { ids, .. } => {
                if let Some(part) = state.part_mut::<TimerState>() {
                    part.cancel(&ids);
                }
            }
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms,
                ..
            } => {
                if let Some(part) = state.part_mut::<TimerState>() {
                    part.fired(&id, next_fire_at_unix_ms);
                }
            }
            _ => {}
        }
    }

    /// Every timer that survived, re-armed with its remaining delay — firing
    /// immediately if it is already due. Whether the agent is parked or was
    /// mid-run, because a timer keeps its promise either way.
    async fn on_load(&mut self, cx: &mut Cx<'_>) {
        let now = now_ms();
        for t in cx.state.timers() {
            spawn_timer_sleep(cx.actor.self_ref(), t.id.clone(), t.remaining(now));
        }
    }
}
