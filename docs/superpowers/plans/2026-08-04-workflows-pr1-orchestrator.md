# Workflows PR 1 — the session-actor orchestrator seam

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure `SessionActor` so a session's topology and its "what happens next" decision are explicit, typed values — with exactly one variant today (`Interactive`) and zero behaviour change — so PR 2 can add `Workflow` without touching the actor's plumbing.

**Architecture:** Three separable moves. (1) `SessionState.subagents` becomes `SessionState.mode: SessionModeState`, a one-variant enum, with a serde migration so snapshots written before this change still load their subagent tree. (2) The actor's `main_agent: Option<..>` + `sub_agents: HashMap<..>` pair becomes a `SessionAgents` enum, removing an `Option` whose `None` means "the instant before spawn". (3) The decision half of `drain`/`flush_owed` moves behind an `Orchestrator` trait as a pure function over `SessionState`, leaving the actor to perform effects.

**Tech Stack:** Rust 2024, `tokio`, `horsie-actor` (event-sourced actors), `serde`, `sqlx`-backed journal.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-04-workflows-design.md`.
- Worktree `.horsie/worktrees/workflows`, branch `feat/workflows`. All commands run from that directory.
- **Zero behaviour change.** Every existing test must pass unmodified except where a test names a field this plan renames.
- CI runs `-D warnings`; `make check` = `fmt-check` + `clippy` + `test`. Run `cargo fmt --all` **before** clippy — a fmt failure masks clippy output.
- `clippy.toml` disallows `horsie_actor::Journal::replay`. Read a journal through the owning actor.
- **Never rename a persisted enum variant or struct field without a migration.** `SessionState` is snapshotted. `SubAgentParent::Main` keeps its name.
- `AppState` is constructed in three places: `server/src/http/mod.rs` tests, `server/src/bin/horsie-server/main.rs`, and twice in `tests/tests/session_server_e2e.rs`.

---

## File Structure

| File | Responsibility |
|---|---|
| `server/src/sessions/mode.rs` | **Create.** `SessionModeState` + its serde migration from legacy snapshots. |
| `server/src/sessions/orchestrator.rs` | **Create.** `Orchestrator` trait, `AgentAction`, `TurnInput`, `SessionCommandKind`, `InteractiveOrchestrator`. |
| `server/src/sessions/session_actor.rs` | **Modify.** `SessionState.mode`; `SessionAgents`; delegate decisions to the orchestrator. |
| `server/src/sessions/mod.rs` | **Modify.** Declare the two new modules. |
| `server/src/http/handlers.rs` | **Modify.** Reads of `state.subagents` become `state.mode`. |

---

### Task 1: `SessionModeState`, with a migration for existing snapshots

`SessionState` is snapshotted into the journal. Moving `subagents` under a new
`mode` field would make every existing snapshot deserialize with an empty
subagent tree — silent data loss on every deployed server. The wire struct below
accepts both shapes.

**Files:**
- Create: `server/src/sessions/mode.rs`
- Modify: `server/src/sessions/mod.rs`
- Modify: `server/src/sessions/session_actor.rs` (the `SessionState` struct and every `state.subagents` use)
- Modify: `server/src/http/handlers.rs`

**Interfaces:**
- Produces: `SessionModeState` (enum, one variant `Interactive { subagents: SubAgentTree }`), `SessionModeState::subagents(&self) -> &SubAgentTree`, `SessionModeState::subagents_mut(&mut self) -> &mut SubAgentTree`.
- Consumes: `crate::sessions::subagents::SubAgentTree`.

- [ ] **Step 1: Write the failing test**

Create `server/src/sessions/mode.rs` with only the test module at first:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::subagents::SubAgentParent;

    /// A snapshot written before `mode` existed carries `subagents` at the top
    /// level. It must load with its tree intact — anything else silently drops
    /// every subagent of every deployed session.
    #[test]
    fn a_pre_mode_snapshot_keeps_its_subagent_tree() {
        let legacy = serde_json::json!({
            "subagents": {
                "nodes": {
                    "3f1a2b4c-0000-4000-8000-000000000001": {
                        "parent": "Main",
                        "label": "reader",
                        "task": "read the file",
                        "depth": 1,
                        "status": "Completed",
                        "output": "done",
                        "error": null,
                        "notified": true
                    }
                }
            }
        });
        let mode: SessionModeState = serde_json::from_value(legacy).unwrap();
        let tree = mode.subagents();
        let id = uuid::Uuid::parse_str("3f1a2b4c-0000-4000-8000-000000000001").unwrap();
        assert_eq!(tree.get(&id).unwrap().label, "reader");
        assert_eq!(tree.get(&id).unwrap().parent, SubAgentParent::Main);
    }

    /// A snapshot with no subagents at all — the overwhelmingly common case.
    #[test]
    fn an_empty_legacy_snapshot_loads_as_interactive() {
        let mode: SessionModeState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(mode.subagents().is_empty());
    }

    /// The new shape round-trips.
    #[test]
    fn the_tagged_shape_round_trips() {
        let mode = SessionModeState::default();
        let json = serde_json::to_value(&mode).unwrap();
        let back: SessionModeState = serde_json::from_value(json).unwrap();
        assert!(back.subagents().is_empty());
    }
}
```

Add to `server/src/sessions/mod.rs`:

```rust
pub mod mode;
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p horsie-server sessions::mode`
Expected: FAIL — `cannot find type SessionModeState in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `server/src/sessions/mode.rs`, above the test module:

```rust
//! What drives a session's agents. One variant today; PR 2 adds `Workflow`.
//!
//! Serialized through [`SessionModeWire`] so that snapshots written before this
//! type existed — which carried `subagents` at the top level of `SessionState`
//! — still load with their tree. `SessionState` is snapshotted into the
//! journal, so its shape is a durability contract.

use crate::sessions::subagents::SubAgentTree;
use serde::{Deserialize, Serialize};

/// What drives a session's agents. Fixed at creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "SessionModeWire", into = "SessionModeWire")]
pub enum SessionModeState {
    /// A person or a routine talks to one resident main agent, which may spawn
    /// subagents.
    Interactive { subagents: SubAgentTree },
}

impl Default for SessionModeState {
    fn default() -> Self {
        Self::Interactive {
            subagents: SubAgentTree::default(),
        }
    }
}

impl SessionModeState {
    /// The subagent tree rooted at this session's main agent.
    pub fn subagents(&self) -> &SubAgentTree {
        match self {
            Self::Interactive { subagents } => subagents,
        }
    }

    /// Mutable access, for the event fold.
    pub fn subagents_mut(&mut self) -> &mut SubAgentTree {
        match self {
            Self::Interactive { subagents } => subagents,
        }
    }
}

/// The serialized shape. `kind` is absent in every snapshot written before this
/// type existed; those carry only `subagents`, and are read as `Interactive`.
/// Written snapshots always carry `kind`, so the fallback is read-only.
#[derive(Serialize, Deserialize)]
struct SessionModeWire {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    subagents: SubAgentTree,
}

impl From<SessionModeWire> for SessionModeState {
    fn from(w: SessionModeWire) -> Self {
        // Only one kind exists; an unknown one from a future version reads as
        // Interactive rather than failing the whole snapshot load.
        Self::Interactive {
            subagents: w.subagents,
        }
    }
}

impl From<SessionModeState> for SessionModeWire {
    fn from(m: SessionModeState) -> Self {
        match m {
            SessionModeState::Interactive { subagents } => Self {
                kind: Some("Interactive".to_string()),
                subagents,
            },
        }
    }
}
```

If `SubAgentTree` does not already derive `Default`, `Clone`, `Debug`,
`Serialize` and `Deserialize`, add the missing ones in
`server/src/sessions/subagents.rs`. If it has no `is_empty`, add:

```rust
impl SubAgentTree {
    /// Whether this tree holds no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p horsie-server sessions::mode`
Expected: PASS, 3 tests.

- [ ] **Step 5: Swap `SessionState.subagents` for `SessionState.mode`**

In `server/src/sessions/session_actor.rs`, in `SessionState`:

```rust
    /// What drives this session's agents, and the subagent tree(s) beneath it.
    #[serde(default)]
    pub mode: SessionModeState,
```

replacing:

```rust
    /// The subagent tree — which agent spawned which, and what became of it.
    #[serde(default)]
    pub subagents: SubAgentTree,
```

Add `use crate::sessions::mode::SessionModeState;` to the imports.

Then update every read and write. Compile-driven: run
`cargo check -p horsie-server` and fix each error. The complete list at the time
of writing:

| Site | Was | Becomes |
|---|---|---|
| `drain` (~:682) | `state.subagents.owed_for(..)` | `state.mode.subagents().owed_for(..)` |
| `flush_owed` (~:774, :775, :782) | `state.subagents.…` | `state.mode.subagents().…` |
| `on_sub_agent_outcome` (~:1060) | `state.subagents.get(&id)` | `state.mode.subagents().get(&id)` |
| `resolve_agent` (~:577) | `state.subagents.get(&id)?` | `state.mode.subagents().get(&id)?` |
| `apply_event` (~:1504-1519) | `state.subagents.apply_*` | `state.mode.subagents_mut().apply_*` |
| `on_recovery_complete` (~:1810) | `state.subagents.interrupted()` | `state.mode.subagents().interrupted()` |
| `SubAgentTree` / `SubAgentStatus` command handlers (~:1660-1695, :1783) | `state.subagents.…` | `state.mode.subagents().…` |
| `server/src/http/handlers.rs` | `snapshot.subagents` / `state.subagents` | via `mode.subagents()` |
| tests in `session_actor.rs` (~:2047-2100) | `s.subagents` | `s.mode.subagents()` |

- [ ] **Step 6: Run the full server suite**

Run: `cargo test -p horsie-server`
Expected: PASS, no test modified except the field accesses above.

- [ ] **Step 7: Commit**

```bash
cd .horsie/worktrees/workflows
cargo fmt --all
git add server/src/sessions/mode.rs server/src/sessions/mod.rs \
        server/src/sessions/session_actor.rs server/src/sessions/subagents.rs \
        server/src/http/handlers.rs
git commit -m "sessions: put the subagent tree behind a session mode"
```

---

### Task 2: `SessionAgents` replaces the `Option` + map pair

`main_agent: Option<ActorRef<AgentCommand>>` documents its `None` as "only in
the instant before" the spawn on recovery. A workflow session has no main agent
at all, so that `None` is about to mean two different things. Make the topology
a value.

**Files:**
- Modify: `server/src/sessions/session_actor.rs`

**Interfaces:**
- Produces: `SessionAgents` (enum: `Interactive { main, subs }`), `SessionAgents::main(&self) -> Option<&ActorRef<AgentCommand>>`, `SessionAgents::sub(&self, id: Uuid) -> Option<&ActorRef<AgentCommand>>`, `SessionAgents::insert_sub(&mut self, id: Uuid, a: ActorRef<AgentCommand>)`, `SessionAgents::drain_all(&mut self) -> Vec<ActorRef<AgentCommand>>`.
- Consumes: Task 1's `SessionModeState`.

- [ ] **Step 1: Write the failing test**

In `session_actor.rs`'s `mod tests`:

```rust
    #[test]
    fn draining_agents_yields_the_main_agent_and_every_sub() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentCommand>(1);
        let main = ActorRef::for_test(tx.clone());
        let mut agents = SessionAgents::interactive(main);
        agents.insert_sub(Uuid::nil(), ActorRef::for_test(tx));
        assert_eq!(agents.drain_all().len(), 2);
        assert!(agents.main().is_none());
    }
```

If `ActorRef` has no test constructor, add one in `actor/src/lib.rs` behind
`#[cfg(any(test, feature = "testing"))]`, or build the refs by spawning the
existing `NoopActor` test fixture already present in `session_actor.rs`'s tests
(around :2190) and reuse that instead — prefer the fixture, no new public API.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p horsie-server draining_agents`
Expected: FAIL — `cannot find type SessionAgents`.

- [ ] **Step 3: Write the implementation**

In `session_actor.rs`, above `struct SessionActor`:

```rust
/// The agent actors a session hosts, resident for its loaded lifetime.
///
/// An enum rather than an `Option` plus a map: a session's topology is decided
/// at creation and never changes, and PR 2's workflow runs have no main agent
/// at all.
enum SessionAgents {
    Interactive {
        main: ActorRef<AgentCommand>,
        subs: HashMap<Uuid, ActorRef<AgentCommand>>,
    },
}

impl SessionAgents {
    fn interactive(main: ActorRef<AgentCommand>) -> Self {
        Self::Interactive {
            main,
            subs: HashMap::new(),
        }
    }

    fn main(&self) -> Option<&ActorRef<AgentCommand>> {
        match self {
            Self::Interactive { main, .. } => Some(main),
        }
    }

    fn sub(&self, id: Uuid) -> Option<&ActorRef<AgentCommand>> {
        match self {
            Self::Interactive { subs, .. } => subs.get(&id),
        }
    }

    fn insert_sub(&mut self, id: Uuid, agent: ActorRef<AgentCommand>) {
        match self {
            Self::Interactive { subs, .. } => {
                subs.insert(id, agent);
            }
        }
    }

    /// Every agent, emptying the set. Used when the session unloads.
    fn drain_all(&mut self) -> Vec<ActorRef<AgentCommand>> {
        match self {
            Self::Interactive { main, subs } => {
                let mut out: Vec<_> = subs.drain().map(|(_, a)| a).collect();
                out.push(main.clone());
                out
            }
        }
    }
}
```

`SessionActor` replaces both fields with `agents: Option<SessionAgents>` — still
an `Option`, but now one that means exactly one thing: *this actor has not
finished recovering yet*. `on_recovery_complete` is the only place that sets it,
via `spawn_main_agent`.

Update the four use sites:

- `fn agent(&self)` → `self.agents.as_ref().and_then(SessionAgents::main)`
- `spawn_main_agent` → `self.agents = Some(SessionAgents::interactive(actor));`
- `spawn_sub_agent_actor` → `if let Some(a) = self.agents.as_mut() { a.insert_sub(id, actor.clone()) }`
- `resolve_agent` / `flush_owed` → `self.agents.as_ref().and_then(|a| a.sub(id)).cloned()`
- `stop_agents` →
  ```rust
  async fn stop_agents(&mut self) {
      let Some(agents) = self.agents.as_mut() else {
          return;
      };
      for agent in agents.drain_all() {
          let _ = agent.tell(AgentCommand::Cancel { ack: None }).await;
          let _ = agent.tell(AgentCommand::Shutdown).await;
      }
      self.agents = None;
  }
  ```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add server/src/sessions/session_actor.rs
git commit -m "sessions: make the hosted agent set a typed value"
```

---

### Task 3: The `Orchestrator` trait and `InteractiveOrchestrator`

`drain` and `flush_owed` currently interleave a decision (*should a turn start,
carrying what?*) with its effects (`tell`, `report`, event construction). Split
the decision out as a pure function so PR 2's workflow variant is a peer rather
than a branch.

**Files:**
- Create: `server/src/sessions/orchestrator.rs`
- Modify: `server/src/sessions/mod.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum SessionCommandKind { UserMessage, Answer, RetryStep }
  pub struct TurnInput { pub message: Option<String>, pub results: Vec<ToolResultInput> }
  pub enum AgentAction {
      StartTurn { who: AgentKey, input: TurnInput, consumed: Vec<String>,
                  answered: Vec<String>, notified: Vec<Uuid>, mark_running: Option<Uuid> },
  }
  pub trait Orchestrator: Send + Sync {
      fn next_actions(&self, state: &SessionState) -> Vec<AgentAction>;
      fn accepts(&self, cmd: SessionCommandKind) -> Result<(), &'static str>;
  }
  pub struct InteractiveOrchestrator;
  ```
- Consumes: `SessionState`, `AgentKey`, `SubAgentParent`, `ToolResultInput`.

- [ ] **Step 1: Write the failing tests**

Create `server/src/sessions/orchestrator.rs` with the test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::session_actor::{InboxMessage, SessionState};
    use horsie_models::session::SessionStatusKind;

    fn with_inbox(texts: &[&str]) -> SessionState {
        let mut s = SessionState::default();
        for (i, t) in texts.iter().enumerate() {
            s.inbox.push(InboxMessage {
                id: format!("m{i}"),
                text: (*t).to_string(),
                at_ms: 0,
            });
        }
        s
    }

    #[test]
    fn an_empty_inbox_starts_nothing() {
        assert!(InteractiveOrchestrator.next_actions(&SessionState::default()).is_empty());
    }

    #[test]
    fn a_queued_message_starts_one_turn_that_consumes_it() {
        let actions = InteractiveOrchestrator.next_actions(&with_inbox(&["hello"]));
        assert_eq!(actions.len(), 1);
        let AgentAction::StartTurn { who, input, consumed, .. } = &actions[0];
        assert_eq!(*who, AgentKey::Main);
        assert_eq!(input.message.as_deref(), Some("hello"));
        assert_eq!(consumed, &vec!["m0".to_string()]);
    }

    /// Anthropic requires alternating roles, so several queued messages merge
    /// into one user turn rather than becoming consecutive user messages.
    #[test]
    fn several_queued_messages_merge_into_one_turn() {
        let actions = InteractiveOrchestrator.next_actions(&with_inbox(&["a", "b"]));
        assert_eq!(actions.len(), 1);
        let AgentAction::StartTurn { input, consumed, .. } = &actions[0];
        assert!(input.message.as_deref().unwrap().contains('a'));
        assert!(input.message.as_deref().unwrap().contains('b'));
        assert_eq!(consumed.len(), 2);
    }

    #[test]
    fn a_running_session_starts_nothing() {
        let mut s = with_inbox(&["hello"]);
        s.status = SessionStatus::Running;
        assert!(InteractiveOrchestrator.next_actions(&s).is_empty());
    }

    #[test]
    fn an_unrecoverable_session_starts_nothing() {
        let mut s = with_inbox(&["hello"]);
        s.status = SessionStatus::Unrecoverable { reason: "gone".into() };
        assert!(InteractiveOrchestrator.next_actions(&s).is_empty());
    }

    /// A message sent while the agent is parked on questions abandons them —
    /// every parked call still gets a result, so nothing dangles on the wire.
    #[test]
    fn a_message_during_a_park_abandons_the_asks() {
        let mut s = with_inbox(&["never mind"]);
        s.pending_asks.push(PendingAsk {
            tool_call_id: Some("call_1".into()),
            question: "which?".into(),
        });
        s.status = SessionStatus::AwaitingInput { asks: s.pending_asks.clone() };
        let actions = InteractiveOrchestrator.next_actions(&s);
        let AgentAction::StartTurn { input, .. } = &actions[0];
        assert_eq!(input.results.len(), 1);
        assert!(input.results[0].is_error);
    }

    #[test]
    fn an_interactive_session_refuses_a_step_retry() {
        assert!(InteractiveOrchestrator.accepts(SessionCommandKind::RetryStep).is_err());
        assert!(InteractiveOrchestrator.accepts(SessionCommandKind::UserMessage).is_ok());
    }
}
```

Add `pub mod orchestrator;` to `server/src/sessions/mod.rs`. `SessionState`,
`InboxMessage`, `PendingAsk` and `SessionStatus` must be `pub` out of
`session_actor` — they already are.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-server sessions::orchestrator`
Expected: FAIL — `cannot find type InteractiveOrchestrator`.

- [ ] **Step 3: Write the implementation**

Prepend to `server/src/sessions/orchestrator.rs`:

```rust
//! What a session does next.
//!
//! The decision is pure — no actors, no I/O, no clock — so it is unit-testable
//! against a hand-built [`SessionState`], and so PR 2's workflow variant is a
//! peer implementation rather than a branch inside the actor. The actor
//! performs whatever this returns.

use crate::sessions::session_actor::{AgentKey, SessionState, SessionStatus};
use crate::sessions::subagents::SubAgentParent;
use horsie_models::agent::ToolResultInput;
use uuid::Uuid;

/// Separator between messages merged into one turn.
pub const MERGE_SEPARATOR: &str = "\n\n";

/// The tool result recorded for an ask the user walked away from.
pub const ABANDONED_ASK_RESULT: &str =
    "The user did not answer and sent a new message instead.";

/// Which command a caller is trying to run, for [`Orchestrator::accepts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommandKind {
    UserMessage,
    Answer,
    RetryStep,
}

/// What an agent is resumed with.
#[derive(Debug, Clone, Default)]
pub struct TurnInput {
    pub message: Option<String>,
    pub results: Vec<ToolResultInput>,
}

/// Something the actor should do. Every field is what the actor needs to
/// journal the action, so the actor never re-derives a decision.
#[derive(Debug, Clone)]
pub enum AgentAction {
    StartTurn {
        who: AgentKey,
        input: TurnInput,
        /// Inbox message ids this turn consumes.
        consumed: Vec<String>,
        /// Ask tool-call ids this turn answers.
        answered: Vec<String>,
        /// Subagents whose results this turn delivers.
        notified: Vec<Uuid>,
        /// A subagent parent this turn puts back into `Running`.
        mark_running: Option<Uuid>,
    },
}

/// Decides what a session does next. Pure.
pub trait Orchestrator: Send + Sync {
    /// Everything startable right now. Called at every turn boundary: a message
    /// arriving while idle, a turn ending, a stop, a subagent finishing.
    fn next_actions(&self, state: &SessionState) -> Vec<AgentAction>;

    /// Whether this session kind takes that command.
    fn accepts(&self, cmd: SessionCommandKind) -> Result<(), &'static str>;
}

/// A person or a routine talking to one resident main agent.
pub struct InteractiveOrchestrator;

impl Orchestrator for InteractiveOrchestrator {
    fn next_actions(&self, state: &SessionState) -> Vec<AgentAction> {
        let mut actions = self.wake_owed_parents(state);
        if let Some(turn) = self.main_turn(state) {
            actions.push(turn);
        }
        actions
    }

    fn accepts(&self, cmd: SessionCommandKind) -> Result<(), &'static str> {
        match cmd {
            SessionCommandKind::UserMessage | SessionCommandKind::Answer => Ok(()),
            SessionCommandKind::RetryStep => Err("this session is not a workflow run"),
        }
    }
}

impl InteractiveOrchestrator {
    /// Wake every idle subagent parent whose children have results it has not
    /// been sent. The main agent is excluded — its owed results merge into its
    /// next turn in [`Self::main_turn`].
    fn wake_owed_parents(&self, state: &SessionState) -> Vec<AgentAction> {
        let tree = state.mode.subagents();
        tree.owed_by_sub_parent()
            .into_iter()
            .filter(|(parent, _)| !tree.is_running(parent))
            .map(|(parent, owed)| AgentAction::StartTurn {
                who: AgentKey::Sub(parent),
                input: TurnInput {
                    message: Some(
                        owed.iter()
                            .map(|(_, text)| text.as_str())
                            .collect::<Vec<_>>()
                            .join(MERGE_SEPARATOR),
                    ),
                    results: Vec::new(),
                },
                consumed: Vec::new(),
                answered: Vec::new(),
                notified: owed.iter().map(|(child, _)| *child).collect(),
                mark_running: Some(parent),
            })
            .collect()
    }

    /// The main agent's turn, if one is owed and no run is in flight.
    fn main_turn(&self, state: &SessionState) -> Option<AgentAction> {
        // Owed subagent results ride every turn the main agent starts; with an
        // empty inbox they can also start one, but only from Idle — never
        // answering a pending ask, never chasing a failure.
        let owed = state.mode.subagents().owed_for(SubAgentParent::Main);
        if state.inbox.is_empty() && (owed.is_empty() || state.status != SessionStatus::Idle) {
            return None;
        }
        if matches!(
            state.status,
            SessionStatus::Running | SessionStatus::Unrecoverable { .. }
        ) {
            return None;
        }
        // One user message, not several: Anthropic requires alternating roles,
        // so consecutive user turns are not portable. Provenance survives in
        // the `MessageQueued` events.
        let mut parts: Vec<&str> = state.inbox.iter().map(|m| m.text.as_str()).collect();
        parts.extend(owed.iter().map(|(_, text)| text.as_str()));
        Some(AgentAction::StartTurn {
            who: AgentKey::Main,
            input: TurnInput {
                message: Some(parts.join(MERGE_SEPARATOR)),
                // A message sent while the agent is parked on questions
                // abandons them: "never mind, do this instead".
                results: state
                    .pending_asks
                    .iter()
                    .filter_map(|ask| ask.tool_call_id.clone())
                    .map(|tool_call_id| ToolResultInput {
                        tool_call_id,
                        output: ABANDONED_ASK_RESULT.to_string(),
                        is_error: true,
                    })
                    .collect(),
            },
            consumed: state.inbox.iter().map(|m| m.id.clone()).collect(),
            answered: Vec::new(),
            notified: owed.iter().map(|(child, _)| *child).collect(),
            mark_running: None,
        })
    }
}
```

Move `MERGE_SEPARATOR` and `ABANDONED_ASK_RESULT` out of `session_actor.rs` and
re-import them from here, so there is one definition of each.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-server sessions::orchestrator`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add server/src/sessions/orchestrator.rs server/src/sessions/mod.rs \
        server/src/sessions/session_actor.rs
git commit -m "sessions: a pure orchestrator decides what runs next"
```

---

### Task 4: `SessionActor` performs the orchestrator's actions

Replace the decision half of `drain` and `flush_owed` with a call to the
orchestrator, leaving the actor holding only effects: `tell`, `report`, and
event construction.

**Files:**
- Modify: `server/src/sessions/session_actor.rs`

**Interfaces:**
- Consumes: Task 3's `Orchestrator`, `AgentAction`, `TurnInput`, `SessionCommandKind`.
- Produces: `SessionActor.orchestrator: Arc<dyn Orchestrator>`; `SessionActor::perform(&mut self, action, ctx) -> Vec<SessionDomainEvent>`.

- [ ] **Step 1: Write the failing test**

In `session_actor.rs`'s `mod tests`, alongside the existing actor tests:

```rust
    /// The actor asks the orchestrator what to do rather than deciding itself:
    /// a queued message produces exactly one TurnBegan naming that message.
    #[tokio::test]
    async fn a_queued_message_produces_one_turn_began_naming_it() {
        let f = actor_fixture().await;
        let id = f
            .session
            .ask(|reply| SessionCommand::UserMessage {
                text: "hello".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        let events = f.journal_events().await;
        let began: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SessionDomainEvent::TurnBegan { consumed, .. } => Some(consumed.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(began, vec![vec![id]]);
    }
```

If the fixture has no `journal_events()` helper, read the journal through
`SessionCommand::History` — `clippy.toml` disallows `Journal::replay`.

- [ ] **Step 2: Run the test to verify it fails or passes for the wrong reason**

Run: `cargo test -p horsie-server a_queued_message_produces_one_turn_began`
Expected: PASS *before* the refactor (it describes existing behaviour). This is
the regression guard for Steps 3–4, not a red test — note that in the commit.

- [ ] **Step 3: Wire the orchestrator in**

Add the field to `SessionActor`:

```rust
    /// Decides what this session runs next. Chosen at construction from the
    /// spec; the actor only performs what it returns.
    orchestrator: Arc<dyn Orchestrator>,
```

In `SessionActor::new`, `orchestrator: Arc::new(InteractiveOrchestrator)`.

Add the performer:

```rust
    /// Carry out one orchestrator decision: resume the agent it names, report
    /// the new status, and return the events that record it.
    async fn perform(
        &mut self,
        action: AgentAction,
        state: &SessionState,
        ctx: &ActorContext<Self>,
    ) -> Vec<SessionDomainEvent> {
        let AgentAction::StartTurn {
            who,
            input,
            consumed,
            answered,
            notified,
            mark_running,
        } = action;
        let agent = match who {
            AgentKey::Main => self.agent().cloned(),
            AgentKey::Sub(id) => match self.agents.as_ref().and_then(|a| a.sub(id)).cloned() {
                Some(agent) => Some(agent),
                // A cold node woken for the first time since load.
                None if state.mode.subagents().get(&id).is_some() => {
                    Some(self.spawn_sub_agent_actor(ctx, id))
                }
                None => None,
            },
        };
        let Some(agent) = agent else {
            return Vec::new();
        };
        // Tell-then-persist: a crash between the agent's resume and this write
        // leaves the result owed, so the next turn re-delivers it. Delivery is
        // at-least-once in that window, never lost.
        if agent
            .tell(AgentCommand::Resume {
                results: input.results,
                message: input.message,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        let mut events = Vec::new();
        match mark_running {
            // A subagent parent waking to consume its children's results.
            Some(parent) => events.push(SessionDomainEvent::SubAgentRunning {
                at_ms: now_ms(),
                id: parent,
            }),
            // The main agent's turn.
            None => {
                self.report(SessionStatus::Running).await;
                events.push(SessionDomainEvent::TurnBegan {
                    at_ms: now_ms(),
                    consumed,
                    answering: None,
                    answered,
                });
            }
        }
        events.extend(
            notified
                .into_iter()
                .map(|id| SessionDomainEvent::SubAgentNotified { at_ms: now_ms(), id }),
        );
        events
    }
```

Replace `flush_then_drain` with:

```rust
    /// Everything the orchestrator wants started at this turn boundary,
    /// performed in order, each seeing the state the previous one produced.
    async fn flush_then_drain(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<Self>,
    ) -> Vec<SessionDomainEvent> {
        let mut events = Vec::new();
        let mut next = state.clone();
        for action in self.orchestrator.next_actions(&next) {
            let produced = self.perform(action, &next, ctx).await;
            for e in &produced {
                next = Self::apply_event(next, e.clone());
            }
            events.extend(produced);
        }
        events
    }
```

Delete `drain` and `flush_owed`; their decision logic now lives in
`InteractiveOrchestrator`. Every existing caller of `flush_then_drain` is
unchanged. `on_answer` keeps its own `TurnBegan` construction — it is a direct
reply to a request, not a boundary decision.

Then gate `UserMessage` on the orchestrator, in `handle_command`:

```rust
            SessionCommand::UserMessage { text, reply } => {
                if let Err(why) = self.orchestrator.accepts(SessionCommandKind::UserMessage) {
                    let _ = reply.send(Err(UserMessageError::Rejected(why.to_string())));
                    return CommandEffect::none();
                }
                self.on_user_message(state, text, reply, ctx).await
            }
```

Add `Rejected(String)` to `UserMessageError`, and map it to **409 Conflict** in
`server/src/http/error.rs` / the `send_message` handler.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test -p horsie-server`
Expected: PASS. Pay particular attention to
`drain_consumes_the_whole_inbox_and_starts_a_turn`,
`drain_abandons_pending_asks_rather_than_answering_them`,
`a_failed_turn_does_not_drain` and
`stop_then_a_queued_message_starts_the_next_turn` — those four pin the exact
behaviour this task moves.

- [ ] **Step 5: Run the full check**

Run: `cargo fmt --all && make check`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
git add server/src/sessions/session_actor.rs server/src/http/error.rs \
        server/src/http/handlers.rs
git commit -m "sessions: the actor performs decisions instead of making them"
```

---

### Task 5: Ship it

- [ ] **Step 1: Run the whole workspace suite**

Run: `cargo fmt --all && make check`
Expected: PASS.

- [ ] **Step 2: Confirm the e2e suite still passes**

Run: `cargo test -p horsie-tests`
Expected: PASS. This suite is serial against one long-lived server; a failure
here that `-p horsie-server` did not catch is almost always a real regression in
recovery or offload.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin feat/workflows
gh pr create --title "sessions: an orchestrator seam, ahead of workflows" --body "$(cat <<'EOF'
Restructures `SessionActor` so a session's topology and its next-action decision are typed values, with one variant today and no behaviour change. Groundwork for workflow runs (`docs/superpowers/specs/2026-08-04-workflows-design.md`).

`SessionState.subagents` moves under `mode: SessionModeState`. Snapshots written before this change carry `subagents` at the top level, so the type deserializes through a wire struct that accepts both — without it every deployed session would silently load with an empty subagent tree.

`main_agent: Option<..>` plus `sub_agents: HashMap<..>` becomes a `SessionAgents` enum. The remaining `Option` now means one thing: recovery has not finished.

The decision half of `drain`/`flush_owed` moves behind a pure `Orchestrator` trait, unit-tested against hand-built states with no actor, runtime or LLM. The actor keeps the effects.
EOF
)"
```

- [ ] **Step 4: Wait for CI green**

Run: `gh pr checks --watch`
Expected: all checks pass.

---

## Self-review notes

- **Spec coverage.** This plan covers the spec's "Session state", "The orchestrator seam" and "Module layout" sections *for the interactive half only*. `AgentKey::Step`, `SessionAgentKind::Step`, `WorkflowRunState`, `StepRun`, the `Orchestrator::on_outcome` hook and per-step `SessionContextProvider.settings` are all deliberately PR 2 — adding them here would be dead code with no producer.
- **Migration.** Task 1 Step 1's first test is the one that matters. Without it this PR is a silent data-loss bug on every deployed server.
- **Type consistency.** `SessionModeState::subagents()` is used in Tasks 1, 3 and 4; `SessionAgents::sub()` in Tasks 2 and 4; `AgentAction::StartTurn`'s six fields are constructed in Task 3 and destructured in Task 4 — the field lists match.
