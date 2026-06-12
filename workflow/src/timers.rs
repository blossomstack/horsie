//! Timer domain types for self-suspending agents.
//!
//! A [`TimerRecord`] is durable agent state: arming one journals it, and it is
//! re-armed from the journal on recovery. Time-derived fields (`fire_at_unix_ms`)
//! are computed once in the actor's command handler and carried in events, never
//! recomputed during the pure `apply_event` fold.

use horsie_agentcore::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds since the Unix epoch. Used for absolute timer fire
/// times so a re-armed timer's remaining delay survives a process restart.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Opaque identifier for one armed timer, unique within an agent session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimerId(pub String);

impl TimerId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for TimerId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TimerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One-shot fires once and is removed; recurring re-arms by `interval_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimerKind {
    OneShot,
    Recurring,
}

/// A single armed timer — durable agent state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerRecord {
    pub id: TimerId,
    pub label: String,
    /// A note the agent leaves for itself, delivered verbatim in the wake message
    /// when the timer fires. Required at the `set_timer` boundary; `#[serde(default)]`
    /// only tolerates legacy journaled timers from before this field existed (they
    /// deserialize to an empty string and yield just the bare fire notice).
    #[serde(default)]
    pub message: String,
    pub kind: TimerKind,
    /// The configured delay; for recurring timers, also the re-arm interval.
    pub interval_secs: u64,
    /// Absolute wall-clock fire time (ms since epoch).
    pub fire_at_unix_ms: u64,
    /// How many times this timer has already fired.
    pub fire_count: u64,
}

impl TimerRecord {
    /// Arm a fresh timer firing `after` from `now_ms`. `message` is delivered
    /// verbatim in the wake message when the timer fires.
    pub fn arm(
        label: String,
        message: String,
        kind: TimerKind,
        after: Duration,
        now_ms: u64,
    ) -> Self {
        Self {
            id: TimerId::new(),
            label,
            message,
            kind,
            interval_secs: after.as_secs(),
            fire_at_unix_ms: now_ms.saturating_add(after.as_millis() as u64),
            fire_count: 0,
        }
    }

    /// Delay from `now_ms` until this timer should fire (zero if already due).
    pub fn remaining(&self, now_ms: u64) -> Duration {
        Duration::from_millis(self.fire_at_unix_ms.saturating_sub(now_ms))
    }

    /// The wake message delivered to the agent when this timer fires. `display_count`
    /// is the 1-based fire number being delivered.
    pub fn wake_message(&self, display_count: u64) -> String {
        let notice = format!("Timer '{}' fired (fire #{display_count}).", self.label);
        if self.message.is_empty() {
            notice
        } else {
            format!("{notice}\n\n{}", self.message)
        }
    }

    /// A render-friendly snapshot for `list_timers`.
    pub fn view(&self, now_ms: u64) -> TimerView {
        TimerView {
            id: self.id.0.clone(),
            label: self.label.clone(),
            message: self.message.clone(),
            kind: match self.kind {
                TimerKind::OneShot => "one_shot",
                TimerKind::Recurring => "recurring",
            },
            interval_secs: self.interval_secs,
            fire_count: self.fire_count,
            fires_in_secs: self.remaining(now_ms).as_secs(),
        }
    }
}

/// A render-friendly view of a timer for the `list_timers` tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimerView {
    pub id: String,
    pub label: String,
    pub message: String,
    pub kind: &'static str,
    pub interval_secs: u64,
    pub fire_count: u64,
    pub fires_in_secs: u64,
}

/// Which timers `cancel_timer` should remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelSelector {
    One(TimerId),
    All,
}

/// The three agent-control timer tools, advertised on top of an agent's toolbox.
pub fn timer_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "set_timer".to_string(),
            description: "Schedule a wake-up. Use it to suspend and be re-prompted later to \
                          re-check external state. Returns a timer id."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind", "after_secs", "label", "message"],
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["one_shot", "recurring"],
                        "description": "one_shot fires once; recurring fires every after_secs."
                    },
                    "after_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Delay in seconds until the timer fires."
                    },
                    "label": {
                        "type": "string",
                        "description": "A short note to yourself, echoed back when it fires."
                    },
                    "message": {
                        "type": "string",
                        "description": "A message to yourself, delivered verbatim each time the \
                                        timer fires — use it to recall the context or instructions \
                                        you need when you wake up."
                    }
                }
            }),
        },
        ToolSpec {
            name: "list_timers".to_string(),
            description: "List your active timers (the reliable source of truth for cancelling)."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolSpec {
            name: "cancel_timer".to_string(),
            description: "Cancel one timer by id, or all of them.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Timer id to cancel." },
                    "all": { "type": "boolean", "description": "Cancel every active timer." }
                }
            }),
        },
    ]
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

    #[test]
    fn arm_sets_fire_at_relative_to_now() {
        let r = TimerRecord::arm(
            "pr".into(),
            String::new(),
            TimerKind::OneShot,
            Duration::from_secs(300),
            1_000,
        );
        assert_eq!(r.fire_at_unix_ms, 1_000 + 300_000);
        assert_eq!(r.interval_secs, 300);
        assert_eq!(r.fire_count, 0);
    }

    #[test]
    fn remaining_is_zero_when_due() {
        let r = TimerRecord::arm(
            "x".into(),
            String::new(),
            TimerKind::OneShot,
            Duration::from_secs(10),
            0,
        );
        assert_eq!(r.remaining(10_000), Duration::ZERO);
        assert_eq!(r.remaining(20_000), Duration::ZERO);
        assert_eq!(r.remaining(4_000), Duration::from_secs(6));
    }

    #[test]
    fn wake_message_includes_label_and_count() {
        let r = TimerRecord::arm(
            "ci".into(),
            String::new(),
            TimerKind::Recurring,
            Duration::from_secs(60),
            0,
        );
        assert_eq!(r.wake_message(3), "Timer 'ci' fired (fire #3).");
    }

    #[test]
    fn wake_message_appends_message_when_present() {
        let r = TimerRecord::arm(
            "ci".into(),
            "recheck the PR's CI status".into(),
            TimerKind::Recurring,
            Duration::from_secs(60),
            0,
        );
        assert_eq!(
            r.wake_message(1),
            "Timer 'ci' fired (fire #1).\n\nrecheck the PR's CI status"
        );
    }

    #[test]
    fn wake_message_falls_back_to_bare_notice_for_empty_message() {
        // Legacy journaled timers (pre-`message`) deserialize to an empty string.
        let r = TimerRecord::arm(
            "ci".into(),
            String::new(),
            TimerKind::OneShot,
            Duration::from_secs(60),
            0,
        );
        assert_eq!(r.wake_message(1), "Timer 'ci' fired (fire #1).");
    }

    #[test]
    fn view_reports_kind_and_remaining() {
        let r = TimerRecord::arm(
            "ci".into(),
            String::new(),
            TimerKind::Recurring,
            Duration::from_secs(60),
            0,
        );
        let v = r.view(10_000);
        assert_eq!(v.kind, "recurring");
        assert_eq!(v.fires_in_secs, 50);
    }

    #[test]
    fn timer_tool_specs_lists_the_three_tools() {
        let names: Vec<_> = timer_tool_specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["set_timer", "list_timers", "cancel_timer"]);
    }
}
