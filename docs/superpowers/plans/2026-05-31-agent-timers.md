# Agent Timers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a workflow agent durable timers — `set_timer` / `list_timers` / `cancel_timer` tools plus a `park` conclude-kind — so it can suspend itself at ~zero cost and be woken later to re-check external state (a PR's CI, an issue, anything pollable), surviving daemon restarts.

**Architecture:** Timers live in the `AgentActor`'s journaled state (one of the existing event-sourced actors). Arming a timer journals a `TimerArmed` event and spawns a `tokio::time::sleep` that sends `TimerFired` back to the actor. A `park` conclude-kind ends the agent's turn while keeping the actor alive (mirroring the existing ask-user pause/resume flow). On fire, the actor resumes the same session with a wake message; if a run is already in flight the wake is coalesced and applied when the run ends. The new `Parked` lifecycle status propagates Agent → Workflow → Job so a parked job auto-resumes (re-arming its timers) after a daemon restart.

**Tech Stack:** Rust, Tokio, the in-house event-sourced `actor` crate, `fluorite` protocol codegen, `serde_json`.

---

## Background: how the pieces fit (read before starting)

Study these existing patterns — the implementation mirrors them:

- **Toolbox dispatch** (`agentcore/src/agent.rs`): the agent loop calls `toolbox.execute(name, input)` for each tool call. The `conclude` tool is the *handoff tool*: it is advertised in `specs()` but **never executed** — when the model calls it, `Agent::run` returns `AgentResult::Handoff { tool_name, data }`. Timer tools (`set_timer`/`list_timers`/`cancel_timer`) are the opposite: they DO execute and return a value, by talking back to the actor.
- **Ask-user pause/resume** (`workflow/src/agent_actor.rs` `handle_finished` → `Conclusion::Ask`, and `workflow/src/workflow_actor.rs` `AgentAsked` → `WorkflowPaused`): the agent calls `conclude(kind=ask)`, the actor tells the workflow `AgentAsked` and **stays alive** (`CommandEffect::snapshot()`), the workflow records `AwaitingUserInput`, and a later `Resume` injects the reply. `park` follows the same skeleton but resumption is driven by a timer, not a human.
- **Backpressure via `ask`** (`workflow/src/agent_actor.rs` `PersistSink`): a component holds an `ActorRef<AgentCommand>` and does `actor.ask(|reply| AgentCommand::X { .., reply }).await`. The actor answers by `reply.send(..)`. Timer tools use this exact pattern.
- **Actor recovery** (`actor/src/runtime.rs` `run_actor`): on spawn, the runtime folds the journal into state, then calls `on_recovery_complete`. `AgentActor::on_recovery_complete` currently re-drives an interrupted turn; we extend it to re-arm timers.
- **Determinism rule** (`actor/src/actor.rs`): `apply_event` is a *pure fold* replayed on recovery. It must NOT read the clock. Any time-derived value (a re-armed timer's next fire time) must be computed in `handle_command` and **carried inside the event**, never recomputed in `apply_event`.

Workspace lints **deny** `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` in non-test code. Never add a `_ =>` arm to a match on a domain enum; handle every variant. Test modules already opt out via the `#![allow(...)]` at the top of each test block — copy that header into any new test module.

Pre-PR gate (run from the worktree root, must be clean):

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
cargo fmt --all --check
```

CI pins **rustc 1.96.0** and builds the PR *merge ref*; if `rustup` has it, prefer `cargo +1.96.0 ...` for the fmt/clippy gate.

---

## File Structure

**New files:**
- `workflow/src/timers.rs` — timer domain types (`TimerId`, `TimerKind`, `TimerRecord`, `TimerView`, `CancelSelector`), the three timer `ToolSpec`s, and the `now_unix_ms` clock helper. Pure, fully unit-tested.

**Modified files:**
- `fluorite/workflow.fl` — add `allow_timers: Option<bool>` to `WorkflowAgentDef`.
- `fluorite/daemon.fl` — add `Parked` to `JobStatus`; add `parked: u32` to `StatusInfo`.
- `workflow/src/lib.rs` — re-export the new public timer types + module.
- `workflow/src/context.rs` — `conclude_tool_spec` gains an `allow_timers` arg and a `park` kind; pass `allow_timers` from the agent def.
- `workflow/src/agent_actor.rs` — the core: `AgentState` timer fields, `AgentDomainEvent` timer/park variants, `apply_event` folds, `AgentCommand` timer variants + handlers, `TimerToolbox`, `interpret` park, `handle_finished` park, `on_recovery_complete` re-arm, `AgentParams::allow_timers`.
- `workflow/src/workflow_actor.rs` — `WorkflowStatus::Parked`, `WorkflowDomainEvent::WorkflowParked`, `WorkflowNotification::Parked`, `WorkflowCommand::AgentParked`, handler + apply + resume + recovery.
- `supervisor/src/job_actor.rs` — `JobDomainEvent::JobParked`, apply, `on_workflow_event` Parked, `on_recovery_complete` Parked relaunch.
- `cli/src/daemon/mod.rs` — `StatusInfo` counter handles `Parked`.
- `cli/src/client.rs` — exit-code match handles `Parked`.
- `workflow/tests/workflow_e2e.rs` — fix the `conclude_tool_spec` call + `WorkflowAgentDef` literals; add timer e2e tests.

---

## Phase 1 — Timer domain (pure, no wiring)

### Task 1: Timer domain types in a new module

**Files:**
- Create: `workflow/src/timers.rs`
- Modify: `workflow/src/lib.rs`

- [ ] **Step 1: Write the failing tests** — create `workflow/src/timers.rs` with this content:

```rust
//! Timer domain types for self-suspending agents.
//!
//! A [`TimerRecord`] is durable agent state: arming one journals it, and it is
//! re-armed from the journal on recovery. Time-derived fields (`fire_at_unix_ms`)
//! are computed once in the actor's command handler and carried in events, never
//! recomputed during the pure `apply_event` fold.

use agentcore::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
    pub kind: TimerKind,
    /// The configured delay; for recurring timers, also the re-arm interval.
    pub interval_secs: u64,
    /// Absolute wall-clock fire time (ms since epoch).
    pub fire_at_unix_ms: u64,
    /// How many times this timer has already fired.
    pub fire_count: u64,
}

impl TimerRecord {
    /// Arm a fresh timer firing `after` from `now_ms`.
    pub fn arm(label: String, kind: TimerKind, after: Duration, now_ms: u64) -> Self {
        Self {
            id: TimerId::new(),
            label,
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
        format!("Timer '{}' fired (fire #{display_count}).", self.label)
    }

    /// A render-friendly snapshot for `list_timers`.
    pub fn view(&self, now_ms: u64) -> TimerView {
        TimerView {
            id: self.id.0.clone(),
            label: self.label.clone(),
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
                "required": ["kind", "after_secs", "label"],
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;

    #[test]
    fn arm_sets_fire_at_relative_to_now() {
        let r = TimerRecord::arm("pr".into(), TimerKind::OneShot, Duration::from_secs(300), 1_000);
        assert_eq!(r.fire_at_unix_ms, 1_000 + 300_000);
        assert_eq!(r.interval_secs, 300);
        assert_eq!(r.fire_count, 0);
    }

    #[test]
    fn remaining_is_zero_when_due() {
        let r = TimerRecord::arm("x".into(), TimerKind::OneShot, Duration::from_secs(10), 0);
        assert_eq!(r.remaining(10_000), Duration::ZERO);
        assert_eq!(r.remaining(20_000), Duration::ZERO);
        assert_eq!(r.remaining(4_000), Duration::from_secs(6));
    }

    #[test]
    fn wake_message_includes_label_and_count() {
        let r = TimerRecord::arm("ci".into(), TimerKind::Recurring, Duration::from_secs(60), 0);
        assert_eq!(r.wake_message(3), "Timer 'ci' fired (fire #3).");
    }

    #[test]
    fn view_reports_kind_and_remaining() {
        let r = TimerRecord::arm("ci".into(), TimerKind::Recurring, Duration::from_secs(60), 0);
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
```

- [ ] **Step 2: Register the module and re-export** — in `workflow/src/lib.rs`, add `mod timers;` (alongside the other `mod` lines) and extend the public re-exports. Find the existing `pub use ...` block that re-exports `conclude_tool_spec` and add the timer types:

```rust
pub use timers::{
    CancelSelector, TimerId, TimerKind, TimerRecord, TimerView, now_unix_ms, timer_tool_specs,
};
```

(Check `workflow/src/lib.rs` for the exact existing `pub use` style and match it. `uuid` is already a dependency of the workflow crate — confirm with `grep uuid workflow/Cargo.toml`; it is used in `agent_actor.rs`.)

- [ ] **Step 3: Run the tests**

Run: `cargo test -p workflow timers::`
Expected: 5 tests pass.

- [ ] **Step 4: Lint + format**

Run: `cargo clippy -p workflow --all-targets -- -D warnings && cargo fmt --all`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add workflow/src/timers.rs workflow/src/lib.rs
git commit -m "feat(workflow): timer domain types"
```

---

## Phase 2 — AgentState journaling for timers + park

### Task 2: Add timer/park state, events, and folds to `AgentActor`

**Files:**
- Modify: `workflow/src/agent_actor.rs`

This task only touches the *data model* (`AgentState`, `AgentDomainEvent`, `apply_event`). No behavior yet — keeps the tree green.

- [ ] **Step 1: Write the failing test** — add to the `tests` module at the bottom of `workflow/src/agent_actor.rs`:

```rust
    #[test]
    fn timer_events_fold_into_state() {
        use crate::timers::{TimerId, TimerKind, TimerRecord};
        use std::time::Duration;

        let rec = TimerRecord::arm("pr".into(), TimerKind::Recurring, Duration::from_secs(60), 0);
        let id = rec.id.clone();
        let mut state = AgentActor::initial_state();

        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: rec });
        assert_eq!(state.timers.len(), 1);

        // Recurring fire re-arms in place with a carried next fire time and bumped count.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired { id: id.clone(), next_fire_at_unix_ms: Some(120_000) },
        );
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.timers[0].fire_count, 1);
        assert_eq!(state.timers[0].fire_at_unix_ms, 120_000);

        // One-shot fire (None) removes it.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired { id, next_fire_at_unix_ms: None },
        );
        assert!(state.timers.is_empty());
    }

    #[test]
    fn park_sets_parked_and_input_clears_it() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, AgentDomainEvent::Parked);
        assert!(state.parked);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage { message: user_msg("wake") },
        );
        assert!(!state.parked);
    }

    #[test]
    fn cancel_event_removes_selected_timers() {
        use crate::timers::{TimerKind, TimerRecord};
        use std::time::Duration;
        let a = TimerRecord::arm("a".into(), TimerKind::OneShot, Duration::from_secs(1), 0);
        let b = TimerRecord::arm("b".into(), TimerKind::OneShot, Duration::from_secs(1), 0);
        let (ia, ib) = (a.id.clone(), b.id.clone());
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: a });
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: b });
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerCancelled { ids: vec![ia] });
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.timers[0].id, ib);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p workflow agent_actor 2>&1 | head -30`
Expected: FAIL — `no variant TimerArmed`, `no field timers`, etc.

- [ ] **Step 3: Extend `AgentState`** — replace the `AgentState` struct (around line 97):

```rust
/// The conversation history reconstructed by folding [`AgentDomainEvent`]s, plus
/// any timers the agent has armed and whether it is currently parked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    pub messages: Vec<Message>,
    /// Active timers — durable so they re-arm on recovery and back `list`/`cancel`.
    #[serde(default)]
    pub timers: Vec<crate::timers::TimerRecord>,
    /// True while the agent has parked itself awaiting a timer (no run in flight).
    #[serde(default)]
    pub parked: bool,
}
```

- [ ] **Step 4: Extend `AgentDomainEvent`** — add four variants to the enum (around line 77). Add them after `RunCancelled`:

```rust
    /// A timer was armed.
    TimerArmed {
        record: crate::timers::TimerRecord,
    },
    /// One or more timers were cancelled.
    TimerCancelled {
        ids: Vec<crate::timers::TimerId>,
    },
    /// A timer fired. `next_fire_at_unix_ms` carries the re-armed fire time for a
    /// recurring timer (so the fold stays pure); `None` removes a one-shot.
    TimerFired {
        id: crate::timers::TimerId,
        next_fire_at_unix_ms: Option<u64>,
    },
    /// The agent parked itself awaiting its timers.
    Parked,
```

- [ ] **Step 5: Extend `apply_event`** — it currently groups `InputMessage | MessageComplete`. Split `InputMessage` out (it must clear `parked`) and add the new arms. Replace the whole `apply_event` body:

```rust
    fn apply_event(mut state: AgentState, event: AgentDomainEvent) -> AgentState {
        match event {
            AgentDomainEvent::InputMessage { message } => {
                // A new turn began — the agent is no longer parked.
                state.parked = false;
                state.messages.push(message);
            }
            AgentDomainEvent::MessageComplete { message } => state.messages.push(message),
            AgentDomainEvent::ToolComplete {
                tool_call_id,
                output,
                is_error,
            } => state
                .messages
                .push(Message::tool_result(tool_call_id, output, is_error)),
            AgentDomainEvent::TimerArmed { record } => state.timers.push(record),
            AgentDomainEvent::TimerCancelled { ids } => {
                state.timers.retain(|t| !ids.contains(&t.id));
            }
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms,
            } => match next_fire_at_unix_ms {
                Some(next) => {
                    if let Some(t) = state.timers.iter_mut().find(|t| t.id == id) {
                        t.fire_at_unix_ms = next;
                        t.fire_count += 1;
                    }
                }
                None => state.timers.retain(|t| t.id != id),
            },
            AgentDomainEvent::Parked => state.parked = true,
            AgentDomainEvent::RunComplete { .. } | AgentDomainEvent::RunCancelled => {}
        }
        state
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p workflow agent_actor`
Expected: PASS (including the three new tests and the existing `apply_event_rebuilds_history_in_order`).

- [ ] **Step 7: Commit**

```bash
git add workflow/src/agent_actor.rs
git commit -m "feat(workflow): journal timer + park state in AgentActor"
```

---

## Phase 3 — Conclude `park` kind + `allow_timers` config

### Task 3: `conclude_tool_spec` gains `allow_timers` and a `park` kind

**Files:**
- Modify: `workflow/src/context.rs`
- Modify: `workflow/tests/workflow_e2e.rs` (caller fix)

- [ ] **Step 1: Write the failing tests** — in `workflow/src/context.rs` tests module, add:

```rust
    #[test]
    fn conclude_without_timers_is_unchanged() {
        // Backward-compat: the no-timers signature still returns None when neither
        // output nor ask is set.
        assert!(conclude_tool_spec(None, false, false).is_none());
    }

    #[test]
    fn conclude_with_timers_offers_park_and_submit() {
        let out = json!({"type": "object"});
        let spec = conclude_tool_spec(Some(&out), false, true).unwrap();
        let kinds = &spec.input_schema["properties"]["kind"]["enum"];
        let kinds: Vec<&str> = kinds.as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert!(kinds.contains(&"submit"));
        assert!(kinds.contains(&"park"));
        assert!(!kinds.contains(&"ask"));
    }

    #[test]
    fn conclude_with_timers_and_ask_offers_all_three() {
        let out = json!({"type": "object"});
        let spec = conclude_tool_spec(Some(&out), true, true).unwrap();
        let kinds = spec.input_schema["properties"]["kind"]["enum"]
            .as_array().unwrap().iter().filter_map(|v| v.as_str()).collect::<Vec<_>>();
        for k in ["submit", "ask", "park"] {
            assert!(kinds.contains(&k), "missing kind {k}");
        }
    }
```

Also update the three existing calls in this test module (lines ~235, 241, 247, 254) to pass the third arg `false`: `conclude_tool_spec(None, false, false)`, `conclude_tool_spec(Some(&out), false, false)`, `conclude_tool_spec(None, true, false)`, `conclude_tool_spec(Some(&out), true, false)`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p workflow context 2>&1 | head -20`
Expected: FAIL — `conclude_tool_spec` takes 2 args, not 3.

- [ ] **Step 3: Rewrite `conclude_tool_spec`** — replace the function (lines ~92-109) with a version that takes `allow_timers` and builds a kind-union whenever timers are on:

```rust
/// Synthesize the `conclude` tool's input schema for an agent. Returns `None` when
/// the agent neither produces structured output, may ask, nor uses timers (it then
/// ends its turn with a plain message).
///
/// With `allow_timers` the tool is always a `kind`-tagged union including `park`
/// (suspend awaiting timers) and `submit` (deliver output), plus `ask` when
/// permitted. Without timers, behavior is exactly as before.
pub fn conclude_tool_spec(
    output_schema: Option<&Value>,
    allow_ask: bool,
    allow_timers: bool,
) -> Option<ToolSpec> {
    let input_schema = if allow_timers {
        timers_kind_schema(output_schema, allow_ask)
    } else {
        match (output_schema, allow_ask) {
            (None, false) => return None,
            (Some(out), false) => out.clone(),
            (None, true) => ask_schema(),
            (Some(out), true) => both_schema(out),
        }
    };
    Some(ToolSpec {
        name: CONCLUDE_TOOL.to_string(),
        description:
            "Finish your turn: deliver final output, ask the user, or park to await your timers."
                .to_string(),
        input_schema,
    })
}

/// Kind-tagged conclude schema for timer-capable agents. Always offers `submit`
/// and `park`; adds `ask` when permitted.
fn timers_kind_schema(output_schema: Option<&Value>, allow_ask: bool) -> Value {
    let mut kinds = vec![json!("submit"), json!("park")];
    if allow_ask {
        kinds.push(json!("ask"));
    }
    json!({
        "type": "object",
        "required": ["kind"],
        "properties": {
            "kind": {
                "type": "string",
                "enum": kinds,
                "description": "submit: deliver final output. park: suspend until a timer fires. ask: pause for user input."
            },
            "output": output_schema.cloned().unwrap_or_else(|| json!({})),
            "question": { "type": "string", "description": "Required when kind=ask." },
            "choices": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional when kind=ask."
            }
        }
    })
}
```

- [ ] **Step 4: Update the factory call** — in `DefaultToolboxFactory::for_agent` (line ~83), pass `allow_timers` from the def:

```rust
        let conclude = conclude_tool_spec(
            agent_def.output_schema.as_ref(),
            agent_def.allow_ask_user,
            agent_def.allow_timers.unwrap_or(false),
        );
```

(`agent_def.allow_timers` does not exist yet — it is added in Task 5's fluorite change. To keep this task compiling on its own, temporarily use `false` here and change it to `agent_def.allow_timers.unwrap_or(false)` in Task 5 Step 4. Note this in the commit.)

Use `false` for now:

```rust
        let conclude =
            conclude_tool_spec(agent_def.output_schema.as_ref(), agent_def.allow_ask_user, false);
```

- [ ] **Step 5: Fix the e2e caller** — in `workflow/tests/workflow_e2e.rs` line ~318, the `BlockingFactory` calls `conclude_tool_spec(def.output_schema.as_ref(), def.allow_ask_user)`. Add the third arg:

```rust
        let conclude = conclude_tool_spec(def.output_schema.as_ref(), def.allow_ask_user, false)
            .expect("worker has an output schema");
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p workflow context && cargo test -p workflow --test workflow_e2e 2>&1 | tail -20`
Expected: context tests PASS; e2e still compiles and passes (it does not use `allow_timers` yet — the `WorkflowAgentDef` literal change comes in Task 5, so e2e may fail to compile until then if the struct gained a field — it has not yet, so it compiles now).

- [ ] **Step 7: Commit**

```bash
git add workflow/src/context.rs workflow/tests/workflow_e2e.rs
git commit -m "feat(workflow): park conclude-kind in conclude_tool_spec"
```

---

## Phase 4 — Timer tools + park behavior in `AgentActor`

This is the core behavioral phase. Split into focused tasks.

### Task 4: `allow_timers` on `AgentParams` + `interpret` park + `Conclusion::Park`

**Files:**
- Modify: `workflow/src/agent_actor.rs`

- [ ] **Step 1: Write the failing test** — add to the `agent_actor.rs` tests module a pure test of `interpret`. `interpret` is a method on `AgentActor`, which needs a context to construct; instead test the decision via a small helper. Add this test that exercises the park-kind decision by constructing `AgentParams` and calling a new pure classifier `classify_conclusion`:

```rust
    #[test]
    fn classify_park_kind_when_timers_enabled() {
        use serde_json::json;
        // timers on: a kind=park payload classifies as Park.
        let c = classify_conclusion(true, true, true, json!({"kind": "park"}), None);
        assert!(matches!(c, Conclusion::Park));
        // kind=submit classifies as Output(output field).
        let c = classify_conclusion(true, true, true, json!({"kind": "submit", "output": {"x": 1}}), None);
        match c {
            Conclusion::Output(v) => assert_eq!(v["x"], 1),
            other => panic!("expected Output, got {other:?}"),
        }
    }
```

Add `#[derive(Debug)]` to the `Conclusion` enum so the test's `panic!("{other:?}")` compiles, and add a `Park` variant.

- [ ] **Step 2: Add `allow_timers` to `AgentParams`** — in the struct (line ~17) add `pub allow_timers: bool,` and in `from_def` (line ~29) add `allow_timers: def.allow_timers.unwrap_or(false),`. (Again, `def.allow_timers` lands in Task 5's fluorite change; to keep this task self-compiling, temporarily set `allow_timers: false,` and switch to `def.allow_timers.unwrap_or(false)` in Task 5.)

Update `handoff_tool` (line ~41):

```rust
    fn handoff_tool(&self) -> Option<String> {
        if self.has_output_schema || self.allow_ask_user || self.allow_timers {
            Some(CONCLUDE_TOOL.to_string())
        } else {
            None
        }
    }
```

- [ ] **Step 3: Add `Park` to `Conclusion` and extract `classify_conclusion`** — change the enum (line ~294):

```rust
#[derive(Debug)]
enum Conclusion {
    Output(Value),
    Ask {
        tool_call_id: Option<String>,
        question: String,
    },
    Park,
}
```

Replace `interpret` (lines ~261-291) so it delegates to a free, pure, testable function:

```rust
    fn interpret(&self, data: Value, tool_call_id: Option<String>) -> Conclusion {
        classify_conclusion(
            self.params.has_output_schema,
            self.params.allow_ask_user,
            self.params.allow_timers,
            data,
            tool_call_id,
        )
    }
```

Add this free function near `interpret` (outside the `impl`):

```rust
/// Classify a `conclude` payload into the agent's terminal intent. With timers the
/// payload is always `kind`-tagged (`submit`/`park`/`ask`); without, it follows the
/// legacy (has_output, allow_ask) shape.
fn classify_conclusion(
    has_output_schema: bool,
    allow_ask_user: bool,
    allow_timers: bool,
    data: Value,
    tool_call_id: Option<String>,
) -> Conclusion {
    let extract_question = |d: &Value| {
        d.get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    if allow_timers {
        let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
        return match kind {
            "park" => Conclusion::Park,
            "ask" => Conclusion::Ask {
                tool_call_id,
                question: extract_question(&data),
            },
            _ => Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null)),
        };
    }
    match (has_output_schema, allow_ask_user) {
        (true, true) => {
            let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
            if kind == "ask" {
                Conclusion::Ask {
                    tool_call_id,
                    question: extract_question(&data),
                }
            } else {
                Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null))
            }
        }
        (true, false) => Conclusion::Output(data),
        (false, true) => Conclusion::Ask {
            tool_call_id,
            question: extract_question(&data),
        },
        (false, false) => Conclusion::Output(data),
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p workflow agent_actor`
Expected: PASS. (`handle_finished` does not yet handle `Conclusion::Park` — add a temporary arm so the match is exhaustive: in `handle_finished`'s `Concluded` branch, the `match self.interpret(...)` must cover `Park`. For now add `Conclusion::Park => CommandEffect::stop()` so it compiles; Task 6 replaces it.)

- [ ] **Step 5: Commit**

```bash
git add workflow/src/agent_actor.rs
git commit -m "feat(workflow): classify park conclude-kind"
```

### Task 5: fluorite `allow_timers` field + wire it through

**Files:**
- Modify: `fluorite/workflow.fl`
- Modify: `workflow/src/context.rs` (revert the temporary `false`)
- Modify: `workflow/src/agent_actor.rs` (revert the temporary `false`)
- Modify: `workflow/tests/workflow_e2e.rs` (struct literal)

> **fluorite gotcha:** do NOT put a doc comment on a union variant (it silently breaks codegen). A doc comment on a *struct field* is fine.

- [ ] **Step 1: Add the field** — in `fluorite/workflow.fl`, inside `struct WorkflowAgentDef`, after `allow_ask_user: bool,` add:

```
    /// Whether the agent may arm timers (set_timer/list_timers/cancel_timer) and
    /// park itself to await them. Optional for back-compat; absent means false.
    allow_timers: Option<bool>,
```

- [ ] **Step 2: Regenerate + verify the field exists**

Run: `cargo build -p models 2>&1 | tail -5`
Expected: builds. Confirm with `grep -rn allow_timers $(find target -path '*models*/out/workflow/mod.rs' | head -1)` that the generated struct has the field. If codegen produced no `mod.rs`, re-check the `.fl` for a stray doc comment on a union variant (see gotcha).

- [ ] **Step 3: Fix all `WorkflowAgentDef` literals** — adding a field breaks every struct literal. Build to find them:

Run: `cargo build -p workflow --all-targets 2>&1 | grep "missing field" | sort -u`

Known sites to add `allow_timers: None,` (or `Some(true)` for timer tests):
- `workflow/src/context.rs` test helper `def(...)` (line ~219).
- `workflow/tests/workflow_e2e.rs` helper `agent(...)` (line ~60).

Add `allow_timers: None,` to each.

- [ ] **Step 4: Revert the temporary `false`s to read the field:**
  - `workflow/src/context.rs` `DefaultToolboxFactory::for_agent`: `agent_def.allow_timers.unwrap_or(false)`.
  - `workflow/src/agent_actor.rs` `AgentParams::from_def`: `allow_timers: def.allow_timers.unwrap_or(false),`.

- [ ] **Step 5: Run the full workflow + models tests**

Run: `cargo test -p models -p workflow 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add fluorite/workflow.fl workflow/src/context.rs workflow/src/agent_actor.rs workflow/tests/workflow_e2e.rs
git commit -m "feat(models): allow_timers on WorkflowAgentDef"
```

### Task 6: Timer commands, `TimerToolbox`, firing, park, and recovery

**Files:**
- Modify: `workflow/src/agent_actor.rs`

This is the behavioral heart. It adds the four timer commands, the toolbox wrapper, the fire/resume/queue logic, the park outcome, and recovery re-arm.

- [ ] **Step 1: Add the four `AgentCommand` variants** — in `enum AgentCommand` (line ~51), add:

```rust
    /// Arm a timer; replies with the new timer id once recorded.
    ArmTimer {
        label: String,
        kind: crate::timers::TimerKind,
        after_secs: u64,
        reply: tokio::sync::oneshot::Sender<crate::timers::TimerId>,
    },
    /// List active timers.
    ListTimers {
        reply: tokio::sync::oneshot::Sender<Vec<crate::timers::TimerView>>,
    },
    /// Cancel one or all timers; replies with the ids actually removed.
    CancelTimer {
        selector: crate::timers::CancelSelector,
        reply: tokio::sync::oneshot::Sender<Vec<crate::timers::TimerId>>,
    },
    /// Internal: a timer's sleep elapsed.
    TimerFired {
        id: crate::timers::TimerId,
    },
```

- [ ] **Step 2: Add a `pending_wake` field** to `AgentActor` (line ~129) and init it `false` in `new`:

```rust
pub struct AgentActor {
    ctx: AgentRuntimeContext,
    params: AgentParams,
    running: Option<CancellationToken>,
    /// A timer fired while a run was in flight; consume it when the run parks.
    pending_wake: bool,
}
```

In `new`: add `pending_wake: false,`.

- [ ] **Step 3: Add a sleep-spawning helper and the `TimerToolbox`** — add near the bottom of the file (before `#[cfg(test)]`):

```rust
/// Spawn a one-shot sleep that tells the actor `TimerFired` after `delay`. The
/// firing is journaled/handled in the actor; a stale fire (timer since cancelled)
/// is ignored there, so an un-cancellable sleep task is harmless.
fn spawn_timer_sleep(
    self_ref: ActorRef<AgentCommand>,
    id: crate::timers::TimerId,
    delay: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = self_ref.tell(AgentCommand::TimerFired { id }).await;
    });
}

/// Wraps an agent's toolbox, adding the three timer control tools. They execute by
/// `ask`ing the owning [`AgentActor`] (never forwarded to the sandboxed runtime).
struct TimerToolbox {
    inner: Arc<dyn Toolbox>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TimerToolbox {
    fn specs(&self) -> Vec<agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(crate::timers::timer_tool_specs());
        specs
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, agentcore::ToolCallError> {
        use agentcore::ToolCallError;
        use crate::timers::{CancelSelector, TimerId, TimerKind};
        match name {
            "set_timer" => {
                let kind = match input.get("kind").and_then(Value::as_str) {
                    Some("one_shot") => TimerKind::OneShot,
                    Some("recurring") => TimerKind::Recurring,
                    _ => {
                        return Err(ToolCallError::InvalidInput(
                            "set_timer.kind must be 'one_shot' or 'recurring'".into(),
                        ));
                    }
                };
                let after_secs = input.get("after_secs").and_then(Value::as_u64).filter(|n| *n >= 1);
                let Some(after_secs) = after_secs else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.after_secs must be an integer >= 1".into(),
                    ));
                };
                let label = input
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let id = self
                    .actor
                    .ask(|reply| AgentCommand::ArmTimer { label, kind, after_secs, reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                Ok(serde_json::json!({ "timer_id": id.0 }))
            }
            "list_timers" => {
                let views = self
                    .actor
                    .ask(|reply| AgentCommand::ListTimers { reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                serde_json::to_value(views).map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))
            }
            "cancel_timer" => {
                let selector = if input.get("all").and_then(Value::as_bool) == Some(true) {
                    CancelSelector::All
                } else if let Some(id) = input.get("id").and_then(Value::as_str) {
                    CancelSelector::One(TimerId(id.to_string()))
                } else {
                    return Err(ToolCallError::InvalidInput(
                        "cancel_timer requires 'id' or 'all': true".into(),
                    ));
                };
                let ids = self
                    .actor
                    .ask(|reply| AgentCommand::CancelTimer { selector, reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                let ids: Vec<String> = ids.into_iter().map(|i| i.0).collect();
                Ok(serde_json::json!({ "cancelled": ids }))
            }
            _ => self.inner.execute(name, input).await,
        }
    }
}
```

- [ ] **Step 4: Wrap the toolbox in `start_run`** — in `start_run` (line ~150), after `let toolbox = self.ctx.toolbox.clone();`, wrap it when timers are enabled. `start_run` already has `let self_ref = ctx.self_ref();` further down — move that binding above the toolbox line, then:

```rust
        let self_ref = ctx.self_ref();
        let toolbox: Arc<dyn Toolbox> = if self.params.allow_timers {
            Arc::new(TimerToolbox {
                inner: self.ctx.toolbox.clone(),
                actor: self_ref.clone(),
            })
        } else {
            self.ctx.toolbox.clone()
        };
```

(Remove the later duplicate `let self_ref = ctx.self_ref();`.)

- [ ] **Step 5: Handle the timer commands** — in `handle_command`, add arms before `AgentCommand::RunFinished`:

```rust
            AgentCommand::ArmTimer { label, kind, after_secs, reply } => {
                let now = crate::timers::now_unix_ms();
                let record = crate::timers::TimerRecord::arm(
                    label,
                    kind,
                    std::time::Duration::from_secs(after_secs),
                    now,
                );
                let id = record.id.clone();
                spawn_timer_sleep(ctx.self_ref(), id.clone(), std::time::Duration::from_secs(after_secs));
                let _ = reply.send(id);
                CommandEffect::persist(vec![AgentDomainEvent::TimerArmed { record }])
            }
            AgentCommand::ListTimers { reply } => {
                let now = crate::timers::now_unix_ms();
                let views = state.timers.iter().map(|t| t.view(now)).collect();
                let _ = reply.send(views);
                CommandEffect::none()
            }
            AgentCommand::CancelTimer { selector, reply } => {
                let ids: Vec<crate::timers::TimerId> = match selector {
                    crate::timers::CancelSelector::All => {
                        state.timers.iter().map(|t| t.id.clone()).collect()
                    }
                    crate::timers::CancelSelector::One(id) => {
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
                    CommandEffect::persist(vec![AgentDomainEvent::TimerCancelled { ids }])
                }
            }
            AgentCommand::TimerFired { id } => self.handle_timer_fired(id, state, ctx).await,
```

- [ ] **Step 6: Implement `handle_timer_fired`** — add as a method on `AgentActor`:

```rust
    /// A timer's sleep elapsed. Re-arm a recurring timer, then resume the agent with
    /// a wake message — unless a run is already in flight, in which case coalesce the
    /// wake and let the run consume it when it parks.
    async fn handle_timer_fired(
        &mut self,
        id: crate::timers::TimerId,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(record) = state.timers.iter().find(|t| t.id == id).cloned() else {
            // Cancelled or already removed — a stale sleep. Ignore.
            return CommandEffect::none();
        };
        let display_count = record.fire_count + 1;
        let now = crate::timers::now_unix_ms();
        // Re-arm recurring; remove one-shot.
        let next_fire_at_unix_ms = match record.kind {
            crate::timers::TimerKind::Recurring => {
                let next = now.saturating_add(record.interval_secs.saturating_mul(1000));
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(record.interval_secs),
                );
                Some(next)
            }
            crate::timers::TimerKind::OneShot => None,
        };
        let fired = AgentDomainEvent::TimerFired { id, next_fire_at_unix_ms };

        if self.running.is_some() {
            // A run is in flight: record the fire (re-arm) and remember to wake when
            // the run parks. Multiple fires coalesce into one wake.
            self.pending_wake = true;
            return CommandEffect::persist(vec![fired]);
        }

        // Idle/parked: start a fresh run with the wake message.
        let wake = AgentInput::user_message(new_message_id(), record.wake_message(display_count));
        let input_event = AgentDomainEvent::InputMessage { message: wake.to_message() };
        self.start_run(wake, ctx, state.messages.clone());
        CommandEffect::persist(vec![fired, input_event])
    }
```

- [ ] **Step 7: Rework `handle_finished` for park + pending wake** — change its signature to take `state` and `ctx`, and replace the temporary `Conclusion::Park` arm. Update the call site in `handle_command`'s `RunFinished` arm to `self.handle_finished(*report, state, ctx).await`. In the `RunOutcome::Concluded` branch, the `match self.interpret(...)` becomes:

```rust
                match self.interpret(data, tool_call_id) {
                    Conclusion::Output(output) => {
                        let _ = parent
                            .tell(WorkflowCommand::AgentConcluded { session_id, output })
                            .await;
                        CommandEffect::stop()
                    }
                    Conclusion::Ask { tool_call_id, question } => {
                        let _ = parent
                            .tell(WorkflowCommand::AgentAsked { session_id, tool_call_id, question })
                            .await;
                        CommandEffect::snapshot()
                    }
                    Conclusion::Park => self.park_or_resume(state, ctx, session_id, parent).await,
                }
```

Add the `park_or_resume` method:

```rust
    /// Decide what a `park` conclusion means: an illegal park (no timers), an
    /// immediate resume (a timer fired during the run), or a real park (stay alive,
    /// status → Parked).
    async fn park_or_resume(
        &mut self,
        state: &AgentState,
        ctx: &ActorContext<Self>,
        session_id: uuid::Uuid,
        parent: ActorRef<WorkflowCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        if state.timers.is_empty() {
            let _ = parent
                .tell(WorkflowCommand::AgentFailed {
                    session_id,
                    error: "agent parked with no active timers — nothing would ever wake it".into(),
                    recoverable: false,
                })
                .await;
            return CommandEffect::stop();
        }
        if self.pending_wake {
            // A timer fired mid-run; go straight back to work instead of parking.
            self.pending_wake = false;
            let wake = AgentInput::user_message(
                new_message_id(),
                "A timer fired while you were busy — re-check now.".to_string(),
            );
            let input_event = AgentDomainEvent::InputMessage { message: wake.to_message() };
            self.start_run(wake, ctx, state.messages.clone());
            return CommandEffect::persist(vec![input_event]);
        }
        let _ = parent
            .tell(WorkflowCommand::AgentParked { session_id })
            .await;
        CommandEffect::persist(vec![AgentDomainEvent::Parked]).and_snapshot()
    }
```

Note: `handle_finished` sets `self.running = None;` at its top — keep that. `park_or_resume` checks `self.pending_wake` which is set by `handle_timer_fired` while `running` was `Some`.

`WorkflowCommand::AgentParked` does not exist yet — Task 7 adds it. To keep this task compiling, Task 7 must land in the same commit OR add the variant first. **Do Task 7 Step 1-2 (add the `AgentParked` command + handler) before building this task.** Reorder if needed; they are interdependent. Simplest: implement Task 7's command/enum additions, then return here.

- [ ] **Step 8: Recovery re-arm** — replace `AgentActor::on_recovery_complete` (line ~380):

```rust
    async fn on_recovery_complete(&mut self, state: &AgentState, ctx: &mut ActorContext<Self>) {
        // Re-arm every surviving timer with its remaining delay (fires immediately if
        // already due). Do this whether parked or mid-run, so timers keep firing.
        let now = crate::timers::now_unix_ms();
        for t in &state.timers {
            spawn_timer_sleep(ctx.self_ref(), t.id.clone(), t.remaining(now));
        }
        // A parked agent waits for a timer — do not re-drive a turn.
        if state.parked {
            return;
        }
        if state.messages.is_empty() {
            return;
        }
        let history = sanitize_for_resume(state.messages.clone());
        self.start_run(
            AgentInput::user_message(new_message_id(), "continue the interrupted task"),
            ctx,
            history,
        );
    }
```

- [ ] **Step 9: Build (expect Task 7 dependency)**

Run: `cargo build -p workflow 2>&1 | grep -E "error|AgentParked" | head`
Expected: only errors about `WorkflowCommand::AgentParked` (resolved by Task 7). Once Task 7's command exists, `cargo build -p workflow` is clean.

- [ ] **Step 10: Commit (with Task 7)** — commit together since they are interdependent:

```bash
git add workflow/src/agent_actor.rs workflow/src/workflow_actor.rs
git commit -m "feat(workflow): timer tools, firing, and park/resume in AgentActor"
```

---

## Phase 5 — Workflow + Job lifecycle (`Parked` status)

### Task 7: `Parked` status in `WorkflowActor`

**Files:**
- Modify: `workflow/src/workflow_actor.rs`

- [ ] **Step 1: Add the command, event, status, notification:**
  - `WorkflowCommand` (line ~14): add `AgentParked { session_id: Uuid },`.
  - `WorkflowDomainEvent` (line ~46): add `WorkflowParked { session_id: Uuid },`.
  - `WorkflowStatus` (line ~78): add `Parked,`.
  - `WorkflowNotification` (line ~93): add `/// The agent parked itself awaiting timers.\n    Parked,`.

- [ ] **Step 2: Handle `AgentParked`** — in `handle_command`, add an arm:

```rust
            WorkflowCommand::AgentParked { session_id } => {
                if !self.is_current(state, session_id) {
                    return CommandEffect::none();
                }
                self.notify(WorkflowNotification::Parked);
                CommandEffect::persist(vec![WorkflowDomainEvent::WorkflowParked { session_id }])
            }
```

- [ ] **Step 3: Fold the event** — in `apply_event`, add:

```rust
            WorkflowDomainEvent::WorkflowParked { .. } => state.status = WorkflowStatus::Parked,
```

- [ ] **Step 4: Make `Parked` resumable + recoverable:**
  - In `on_resume` (line ~354), the `match state.status` is exhaustive over `WorkflowStatus`. Add a `Parked` arm that behaves like `Suspended` (re-spawn if needed, `tell(Run { message })`). The cleanest is to merge: change `WorkflowStatus::Suspended =>` to `WorkflowStatus::Suspended | WorkflowStatus::Parked =>`. Also remove `Parked` from any catch-all — the final arm `Pending | Running | Finished | Failed` stays as-is (do not add `Parked`).
  - In `on_recovery_complete` (line ~648), change the guard so parked workflows also re-spawn their agent (the agent then re-arms its timers):

```rust
    async fn on_recovery_complete(&mut self, state: &WorkflowState, ctx: &mut ActorContext<Self>) {
        if !matches!(state.status, WorkflowStatus::Running | WorkflowStatus::Parked) {
            return;
        }
        let (Some(agent_name), Some(session_id)) =
            (state.current_agent.clone(), state.current_session_id)
        else {
            return;
        };
        let Some(agent_def) = self.agent_def(&agent_name).cloned() else {
            return;
        };
        if let Ok(child) = self.spawn_agent(ctx, &agent_def, session_id) {
            self.current_child = Some(child);
        }
    }
```

- [ ] **Step 5: Add a unit test** — in `workflow_actor.rs` tests:

```rust
    #[test]
    fn parked_event_sets_parked_status() {
        let s = WorkflowActor::apply_event(
            WorkflowActor::initial_state(),
            WorkflowDomainEvent::WorkflowParked { session_id: sess() },
        );
        assert_eq!(s.status, WorkflowStatus::Parked);
    }
```

- [ ] **Step 6: Build the workflow crate (now Task 6 resolves too)**

Run: `cargo build -p workflow --all-targets 2>&1 | tail -20`
Expected: clean. Then `cargo test -p workflow` → PASS.

- [ ] **Step 7: Commit** — see Task 6 Step 10 (committed together).

### Task 8: `Parked` status in `JobActor` + recovery relaunch

**Files:**
- Modify: `fluorite/daemon.fl`
- Modify: `supervisor/src/job_actor.rs`

- [ ] **Step 1: Add to the protocol** — in `fluorite/daemon.fl`:
  - `enum JobStatus` (line ~10): add `Parked,` after `AwaitingUserInput,`.
  - `struct StatusInfo` (line ~60): add `parked: u32,` after `suspended: u32,`.

- [ ] **Step 2: Regenerate**

Run: `cargo build -p models 2>&1 | tail -5`
Expected: builds.

- [ ] **Step 3: Add the job event + fold** — in `supervisor/src/job_actor.rs`:
  - `JobDomainEvent` (line ~299): add `JobParked,`.
  - `apply_event` (line ~443): add `JobDomainEvent::JobParked => JobStatus::Parked,`.

- [ ] **Step 4: Handle the workflow notification** — in `on_workflow_event` (line ~399), add an arm. A parked job keeps its workflow + runtime alive (the agent will wake and may run tools), so do NOT teardown:

```rust
            WorkflowNotification::Parked => {
                self.report(JobStatus::Parked).await;
                CommandEffect::persist(vec![JobDomainEvent::JobParked])
            }
```

- [ ] **Step 5: Auto-resume parked jobs on restart** — in `on_recovery_complete` (line ~510), a parked job must relaunch its workflow (which re-spawns the agent, which re-arms timers). Add `Parked` to the relaunch arm:

```rust
        match state.status {
            Some(JobStatus::Running) | Some(JobStatus::Parked) => {
                if let Err(e) = self.launch_workflow(Kickoff::Recover, ctx).await {
                    tracing::error!(job_id = %self.job_id, error = %e, "failed to recover job");
                }
            }
            None
            | Some(JobStatus::Suspended)
            | Some(JobStatus::AwaitingUserInput)
            | Some(JobStatus::Finished)
            | Some(JobStatus::Failed) => {}
        }
```

- [ ] **Step 6: Add a unit test** — in `job_actor.rs` tests:

```rust
    #[test]
    fn parked_event_sets_parked_status() {
        let s = JobActor::apply_event(JobState::default(), JobDomainEvent::JobParked);
        assert_eq!(s.status, Some(JobStatus::Parked));
    }
```

- [ ] **Step 7: Build supervisor**

Run: `cargo build -p supervisor --all-targets 2>&1 | tail -20`
Expected: clean (the `is_terminal` helper at supervisor_actor.rs:79 uses `matches!(.., Finished | Failed)`, which correctly returns false for `Parked` — no change needed).

- [ ] **Step 8: Commit**

```bash
git add fluorite/daemon.fl supervisor/src/job_actor.rs
git commit -m "feat(supervisor): Parked job status with restart auto-resume"
```

### Task 9: CLI ripple (`Parked` in status counter + exit code)

**Files:**
- Modify: `cli/src/daemon/mod.rs`
- Modify: `cli/src/client.rs`

- [ ] **Step 1: Build to find the broken matches**

Run: `cargo build -p cli --all-targets 2>&1 | grep -A3 "non-exhaustive\|not covered" | head -40`
Expected: two sites — the `StatusInfo` counter (daemon/mod.rs ~207) and the exit-code match (client.rs ~165).

- [ ] **Step 2: Status counter** — in `cli/src/daemon/mod.rs`:
  - Add `parked: 0,` to the `StatusInfo { .. }` initializer (line ~198).
  - Add `Parked` to the `use models::daemon::JobStatus::{..}` import (line ~207) and a match arm:

```rust
                match j.status {
                    Running => info.running += 1,
                    Parked => info.parked += 1,
                    Suspended | AwaitingUserInput => info.suspended += 1,
                    Finished => info.finished += 1,
                    Failed => info.failed += 1,
                }
```

- [ ] **Step 3: Exit code** — in `cli/src/client.rs` `run_attached` (line ~165), a parked job is non-failed (exit 0). Add it to the `0` group:

```rust
    Ok(match status {
        Some(JobStatus::Failed) => 1,
        Some(JobStatus::Running)
        | Some(JobStatus::Parked)
        | Some(JobStatus::Suspended)
        | Some(JobStatus::AwaitingUserInput)
        | Some(JobStatus::Finished)
        | None => 0,
    })
```

- [ ] **Step 4: Check the status printer** — if `cli/src/client.rs` `status()` prints `StatusInfo` fields, add a line for `parked`. Inspect:

Run: `grep -n "running\|suspended\|finished\|failed" cli/src/client.rs cli/src/main.rs | grep -i print`
If a human-readable status print exists, add the `parked` count line there matching the existing format. If it only prints via `Debug`/serde, no change needed.

- [ ] **Step 5: Build the whole workspace**

Run: `cargo build --workspace --all-targets 2>&1 | tail -20`
Expected: clean. Fix any remaining exhaustive-match sites the compiler flags (e.g. another `JobStatus` match) by handling `Parked` explicitly.

- [ ] **Step 6: Commit**

```bash
git add cli/src/daemon/mod.rs cli/src/client.rs
git commit -m "feat(cli): surface Parked job status"
```

---

## Phase 6 — End-to-end tests

### Task 10: e2e — arm, park, fire, resume, finish

**Files:**
- Modify: `workflow/tests/workflow_e2e.rs`

These run the real actor stack with a mock LLM. Use a 1-second one-shot timer (well within the 5s `wait_for_status` budget) so firing is observed without controlling time.

- [ ] **Step 1: Add a timer-capable agent helper** — at the top of the test file, add:

```rust
fn timer_agent(name: &str) -> WorkflowAgentDef {
    let mut a = agent(name);
    a.allow_timers = Some(true);
    a.allowed_tools = None; // allow the timer tools (allowlist None = all)
    a
}
```

- [ ] **Step 2: Add the park→fire→finish test:**

```rust
#[tokio::test]
async fn timer_parks_then_fires_and_resumes() {
    // Turn 1: arm a 1s one-shot, then park. Turn 2 (after it fires): submit + finish.
    let mock = MockLlmServer::builder()
        .tool_call("set_timer", json!({"kind": "one_shot", "after_secs": 1, "label": "pr"}))
        .tool_call(CONCLUDE_TOOL, json!({"kind": "park"}))
        .tool_call(CONCLUDE_TOOL, json!({"kind": "submit", "output": {"done": true}}))
        .build()
        .await;

    let def = WorkflowDefinition {
        start: "watcher".into(),
        agents: vec![timer_agent("watcher")],
    };

    let journal = Arc::new(InMemoryJournal::new());
    let (rt, mut events) =
        runtime_context(provider_at(&mock.url()), Arc::new(DefaultToolboxFactory));
    let wf = spawn_root(WorkflowActor::new("wf-timer", def, rt), journal.clone());

    wf.tell(WorkflowCommand::Start { input: "watch the PR".into() })
        .await
        .unwrap();

    // It parks first…
    wait_for_status(&journal, "wf-timer", WorkflowStatus::Parked).await;
    let n = recv_notification(&mut events).await;
    assert!(matches!(n, WorkflowNotification::Parked));

    // …then the 1s timer fires, the agent resumes and finishes.
    let state = wait_for_status(&journal, "wf-timer", WorkflowStatus::Finished).await;
    assert_eq!(state.current_agent.as_deref(), Some("watcher"));
}
```

- [ ] **Step 3: Add a park-with-no-timers failure test:**

```rust
#[tokio::test]
async fn park_without_timers_fails_the_run() {
    let mock = MockLlmServer::builder()
        .tool_call(CONCLUDE_TOOL, json!({"kind": "park"}))
        .build()
        .await;
    let def = WorkflowDefinition {
        start: "watcher".into(),
        agents: vec![timer_agent("watcher")],
    };
    let journal = Arc::new(InMemoryJournal::new());
    let (rt, _events) = runtime_context(provider_at(&mock.url()), Arc::new(DefaultToolboxFactory));
    let wf = spawn_root(WorkflowActor::new("wf-nopark", def, rt), journal.clone());
    wf.tell(WorkflowCommand::Start { input: "go".into() }).await.unwrap();
    let state = wait_for_status(&journal, "wf-nopark", WorkflowStatus::Failed).await;
    assert_eq!(state.status, WorkflowStatus::Failed);
}
```

- [ ] **Step 4: Run the e2e tests**

Run: `cargo test -p workflow --test workflow_e2e 2>&1 | tail -25`
Expected: all PASS (existing + 2 new). If `timer_parks_then_fires_and_resumes` is flaky on slow machines, bump the `wait_for_status` budget is not needed (it is 5s); ensure `after_secs: 1`.

- [ ] **Step 5: Commit**

```bash
git add workflow/tests/workflow_e2e.rs
git commit -m "test(workflow): e2e timer park/fire/resume"
```

### Task 11: e2e — recurring timer survives a fire; cancel works

**Files:**
- Modify: `workflow/tests/workflow_e2e.rs`

- [ ] **Step 1: Add a recurring + cancel test** — the agent arms a recurring 1s timer and parks; after it fires once it lists timers (still present, fire_count ≥ 1), cancels all, then submits:

```rust
#[tokio::test]
async fn recurring_timer_fires_then_can_be_cancelled() {
    let mock = MockLlmServer::builder()
        .tool_call("set_timer", json!({"kind": "recurring", "after_secs": 1, "label": "ci"}))
        .tool_call(CONCLUDE_TOOL, json!({"kind": "park"}))
        // wake #1: confirm it is still listed, then cancel + finish.
        .tool_call("list_timers", json!({}))
        .tool_call("cancel_timer", json!({"all": true}))
        .tool_call(CONCLUDE_TOOL, json!({"kind": "submit", "output": {"done": true}}))
        .build()
        .await;

    let def = WorkflowDefinition {
        start: "watcher".into(),
        agents: vec![timer_agent("watcher")],
    };
    let journal = Arc::new(InMemoryJournal::new());
    let (rt, _events) = runtime_context(provider_at(&mock.url()), Arc::new(DefaultToolboxFactory));
    let wf = spawn_root(WorkflowActor::new("wf-recur", def, rt), journal.clone());
    wf.tell(WorkflowCommand::Start { input: "watch".into() }).await.unwrap();

    wait_for_status(&journal, "wf-recur", WorkflowStatus::Parked).await;
    let state = wait_for_status(&journal, "wf-recur", WorkflowStatus::Finished).await;
    assert_eq!(state.status, WorkflowStatus::Finished);

    // The agent session journaled a TimerArmed and at least one TimerFired.
    let session_id = state.current_session_id.unwrap();
    let pid = PersistenceId::new("agent", session_id.to_string());
    let mut armed = 0;
    let mut fired = 0;
    let mut ev = journal.replay(&pid, 0).await;
    while let Some(item) = ev.next().await {
        match serde_json::from_slice::<AgentDomainEvent>(&item.unwrap()).unwrap() {
            AgentDomainEvent::TimerArmed { .. } => armed += 1,
            AgentDomainEvent::TimerFired { .. } => fired += 1,
            _ => {}
        }
    }
    assert!(armed >= 1 && fired >= 1, "expected the recurring timer to arm and fire");
}
```

(Confirm `MockResponse`/`Scenario` serves tool-call responses in order and loops or stops appropriately — see `providers/mock-llm/src/server.rs`. If the builder requires every `complete` call to have a queued response, ensure the count matches the agent's turns. The agent makes one provider call per turn: arm(1) is one turn returning the set_timer call; the loop then executes set_timer and calls again → returns park. So each `.tool_call(..)` is one provider response consumed per turn in order.)

- [ ] **Step 2: Run**

Run: `cargo test -p workflow --test workflow_e2e recurring 2>&1 | tail -25`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add workflow/tests/workflow_e2e.rs
git commit -m "test(workflow): e2e recurring fire + cancel"
```

### Task 12: supervisor e2e — parked job recovers after restart

**Files:**
- Modify: `supervisor/tests/daemon_e2e.rs`

- [ ] **Step 1: Study the existing recovery test** — read `supervisor/tests/daemon_e2e.rs` around line 339-351 (`AwaitingUserInput` recovery: it stops the supervisor, re-spawns from the same journal, and asserts the recovered status). Mirror that structure for a parked job.

- [ ] **Step 2: Add a parked-recovers test** — using the same mock-LLM + supervisor harness the file already sets up, script an agent that arms a long (e.g. 3600s) recurring timer and parks; assert the job reaches `JobStatus::Parked`; then simulate a restart (drop + re-spawn the supervisor on the same journal) and assert the job is relaunched (status stays `Parked` and the workflow/agent are live again — assert via the supervisor's `List`/status fold exactly as the awaiting-input test does). Use a long timer so it does not fire during the test, isolating the recovery behavior.

Follow the precise harness calls (`wait_for`, supervisor spawn, journal reuse) from the existing `AwaitingUserInput` recovery test verbatim — only the scripted scenario and the asserted status differ.

- [ ] **Step 3: Run**

Run: `cargo test -p supervisor --test daemon_e2e 2>&1 | tail -25`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add supervisor/tests/daemon_e2e.rs
git commit -m "test(supervisor): parked job auto-resumes after restart"
```

---

## Phase 7 — Final gate + PR

### Task 13: Full verification gate

- [ ] **Step 1: Format**

Run: `cargo fmt --all`

- [ ] **Step 2: Single combined gate** — run everything and confirm a clean finish (per the project's "verify before push" rule, one combined gate):

```bash
cargo build --workspace --all-targets \
  && cargo clippy --all-targets --all-features -- -D warnings \
  && cargo test --workspace \
  && cargo fmt --all --check \
  && echo GATE_GREEN
```

Expected: ends with `GATE_GREEN`. If `cargo deny` is part of CI, also run `cargo deny check` if available locally. No new crates were added, so license/publish gates are unaffected.

- [ ] **Step 3: 1.96.0 parity (if available)** — CI pins rustc 1.96.0:

```bash
rustup toolchain list | grep -q 1.96.0 && cargo +1.96.0 clippy --all-targets --all-features -- -D warnings && cargo +1.96.0 fmt --all --check || echo "1.96.0 not installed; rely on CI"
```

- [ ] **Step 4: Update the spec's status** — edit `docs/superpowers/specs/2026-05-31-agent-timers-design.md` header `Status:` to `Implemented`. Commit:

```bash
git add docs/superpowers/specs/2026-05-31-agent-timers-design.md
git commit -m "docs: mark agent timers design implemented"
```

### Task 14: Open the PR

- [ ] **Step 1: Push the branch** (the work is on a worktree branch; confirm with `git status` / `git branch --show-current`).

```bash
git push -u origin HEAD
```

- [ ] **Step 2: Open the PR** — succinct body (what / why / how-tested), no AI attribution:

```bash
gh pr create --title "feat: agent timers (self-suspending park/wake)" --body "$(cat <<'EOF'
## What
Durable agent timers: `set_timer` / `list_timers` / `cancel_timer` tools plus a `park` conclude-kind, letting an agent suspend itself at ~zero cost and be woken later to re-check external state (PR CI, issues, anything pollable).

## Why
Closes the loop for agents that open a PR (or watch any resource) and must wait for asynchronous results without holding an LLM context the whole time.

## How it works
- Timers live in the `AgentActor`'s journaled state; arming spawns a `tokio` sleep that messages the actor on fire. `apply_event` stays pure (re-armed fire times are carried in events).
- `park` ends the turn while keeping the actor alive (mirrors the ask-user pause/resume flow). On fire the same session resumes with a wake message; a fire during a run is coalesced and applied when the run parks.
- A `Parked` lifecycle status flows Agent → Workflow → Job, so a parked job auto-resumes and re-arms its timers after a daemon restart.

## Tested
- Unit: timer domain, event folds, conclude-kind classification, status folds.
- e2e (`workflow`): park→fire→resume→finish; park-with-no-timers fails; recurring fire + cancel.
- e2e (`supervisor`): parked job auto-resumes after a simulated restart.

Out of scope (follow-ups): cron / absolute-time schedules; a global scheduler.
EOF
)"
```

- [ ] **Step 3: Watch CI to green**

```bash
gh pr view --json number -q .number   # note the PR number
gh pr checks --watch
```

If a check fails, read the log (`gh run view <run-id> --log-failed`), fix, commit, push, and re-watch. Do not stop until checks are green.

---

## Self-Review notes (already applied)

- **Spec coverage:** tools (Task 1,6) · journaling (Task 2) · park kind (Task 3,4) · fire/queue (Task 6) · recovery re-arm (Task 6,8,12) · Parked status across layers (Task 7,8,9) · no cron / no global scheduler (explicitly out of scope). All spec sections map to a task.
- **Determinism:** re-armed fire times are carried in `TimerFired` events, never recomputed in `apply_event` (Phase 2, Task 6 Step 6).
- **Interdependency:** Task 6 (`AgentParked` tell) and Task 7 (the command/handler) must build together — called out in Task 6 Step 7/10.
- **Lints:** every new domain-enum match is exhaustive; test modules carry the `#![allow(...)]` header; no `unwrap`/`expect`/`panic` in non-test code (timer parsing returns `ToolCallError`, time helper uses `unwrap_or`).
- **Back-compat:** `allow_timers` is `Option<bool>` and new `AgentState` fields use `#[serde(default)]`, so old journals/workflows still deserialize.
