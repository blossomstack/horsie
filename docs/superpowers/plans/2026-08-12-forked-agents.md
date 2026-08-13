# Forked agents implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a conversation branch into a second, user-facing conversation inside the same session — seeded either by copying the history or by summarising it.

**Architecture:** A fork is a fourth kind of agent hosted by the session (`SessionAgentKind::Fork`), beside `Main`, `Sub` and `Step`. It takes the main agent's toolbox layers so it can `ask_user`, rename itself and spawn subagents. Its roster lives in `SessionState.forks`, owned by a new `ForkedAgents` component. `/fork` and `/summary-n-fork` are server-owned builtins resolved in `Turns`, which hands the work to `ForkedAgents`.

**Tech Stack:** Rust (tokio, `horsie-actor` event-sourced actors, sqlx), fluorite schemas with TypeScript codegen, React + TanStack Query + Playwright.

**Spec:** `docs/superpowers/specs/2026-08-12-forked-agents-design.md`

## Global Constraints

- **Work in the worktree** `/Users/xiaoguang/works/repos/bloomstack/october/horsie/.claude/worktrees/forked-agents`, branch `feat/forked-agents`.
- **Journal state is a durability contract.** Every new field on a persisted struct carries `#[serde(default)]`. Never rename or repurpose an existing field or event variant — renaming persisted variants took the supervisor down on 2026-08-02.
- **`.fl` edits require `make types`.** `crates/models/fluorite/*.fl` generates both Rust and `clients/web/src/generated/**`. A `.fl` change without regenerating leaves the two trees disagreeing.
- **Wire JSON is camelCase.** A hand-written snake_case key in a test body is silently ignored.
- **Verify with `cargo test -p <crate> --lib`** while iterating. Run the full workspace suite once before pushing, never twice.
- **Never `git -c user.name=` / `user.email=`.** A wrong identity fails the CLA check.
- **`begin_write()` for every write transaction.** A deferred read→write lock cannot be retried.
- Agent id `"main"` is `crate::sessions::MAIN_AGENT_ID`.

---

### Task 1: The fork roster (pure data)

**Files:**
- Create: `crates/server/src/sessions/forks.rs`
- Modify: `crates/server/src/sessions/mod.rs` (add `pub mod forks;`)

**Interfaces:**
- Produces: `ForkParent`, `ForkMode`, `ForkRecord`, `ForkRoster` with `apply_created`, `apply_seeded`, `apply_titled`, `apply_status`, `apply_deleted`, `get`, `contains`, `iter`, `seeding`, `has_seeding`.

- [ ] **Step 1: Write the failing tests**

```rust
// at the bottom of crates/server/src/sessions/forks.rs
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    #[test]
    fn a_created_fork_starts_provisioning_and_unnamed() {
        let mut r = ForkRoster::default();
        r.apply_created(id(1), ForkParent::Main, 42, ForkMode::Copy, 1_000);
        let rec = r.get(id(1)).unwrap();
        assert_eq!(rec.parent, ForkParent::Main);
        assert_eq!(rec.source_seq, 42);
        assert_eq!(rec.mode, ForkMode::Copy);
        assert_eq!(rec.title, None);
        assert_eq!(rec.status, AgentStatus::Provisioning);
        assert_eq!(rec.created_at_ms, 1_000);
    }

    /// Seeding is what ends the provisioning window: until it lands there is
    /// nothing for the fork to run against.
    #[test]
    fn seeding_moves_a_fork_to_idle() {
        let mut r = ForkRoster::default();
        r.apply_created(id(1), ForkParent::Main, 0, ForkMode::Summary, 1_000);
        assert!(r.has_seeding(), "a fork awaiting its seed keeps the session loaded");
        r.apply_seeded(id(1));
        assert_eq!(r.get(id(1)).unwrap().status, AgentStatus::Idle);
        assert!(!r.has_seeding());
        assert!(r.seeding().is_empty());
    }

    #[test]
    fn a_fork_names_itself() {
        let mut r = ForkRoster::default();
        r.apply_created(id(1), ForkParent::Main, 0, ForkMode::Copy, 1_000);
        r.apply_titled(id(1), "Try the other migration".to_string());
        assert_eq!(
            r.get(id(1)).unwrap().title.as_deref(),
            Some("Try the other migration")
        );
    }

    #[test]
    fn a_fork_of_a_fork_records_its_parent() {
        let mut r = ForkRoster::default();
        r.apply_created(id(1), ForkParent::Main, 0, ForkMode::Copy, 1_000);
        r.apply_created(id(2), ForkParent::Fork(id(1)), 7, ForkMode::Copy, 2_000);
        assert_eq!(r.get(id(2)).unwrap().parent, ForkParent::Fork(id(1)));
    }

    #[test]
    fn deleting_a_fork_leaves_its_siblings() {
        let mut r = ForkRoster::default();
        r.apply_created(id(1), ForkParent::Main, 0, ForkMode::Copy, 1_000);
        r.apply_created(id(2), ForkParent::Main, 0, ForkMode::Copy, 2_000);
        r.apply_deleted(id(2));
        assert!(r.contains(id(1)));
        assert!(!r.contains(id(2)));
    }

    /// Deleting a fork orphans nothing: a child fork keeps its own transcript,
    /// and a parent id that no longer resolves renders at the top level.
    #[test]
    fn deleting_a_parent_fork_leaves_its_child() {
        let mut r = ForkRoster::default();
        r.apply_created(id(1), ForkParent::Main, 0, ForkMode::Copy, 1_000);
        r.apply_created(id(2), ForkParent::Fork(id(1)), 0, ForkMode::Copy, 2_000);
        r.apply_deleted(id(1));
        assert!(r.contains(id(2)), "a child fork is its own conversation");
    }

    /// A fold applied to an id that is gone must not resurrect it: events for a
    /// deleted fork can still be in flight when the delete lands.
    #[test]
    fn events_for_a_deleted_fork_are_ignored() {
        let mut r = ForkRoster::default();
        r.apply_created(id(1), ForkParent::Main, 0, ForkMode::Copy, 1_000);
        r.apply_deleted(id(1));
        r.apply_seeded(id(1));
        r.apply_titled(id(1), "ghost".to_string());
        r.apply_status(id(1), AgentStatus::Running);
        assert!(!r.contains(id(1)));
    }

    /// Pre-fork journal rows have no `forks` key at all.
    #[test]
    fn an_absent_roster_deserializes_empty() {
        let r: ForkRoster = serde_json::from_str("{}").unwrap();
        assert_eq!(r.iter().count(), 0);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib forks::`
Expected: FAIL — `could not find 'forks' in 'sessions'`

- [ ] **Step 3: Write the implementation**

```rust
//! The session's forks: which conversation each branched from, and where each
//! one has got to. Pure data — the session actor folds its journal events
//! through these methods, so live operation and recovery follow one path.
//!
//! Deliberately not a `SubAgentTree`. That structure's whole vocabulary —
//! `notified`, `TreeOwner`, `owed_deliveries` — exists to guarantee a parent
//! eventually receives a child's result. A fork owes nobody one, so putting it
//! there would mean carrying fields that must always be inert.

use crate::sessions::session_actor::AgentStatus;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The agent a fork was taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ForkParent {
    /// The session's main agent.
    Main,
    /// Another fork. Forks nest arbitrarily — a person types `/fork`, so there
    /// is no runaway to bound the way `MAX_SUBAGENT_DEPTH` bounds a machine.
    Fork(Uuid),
}

/// How a fork's history was seeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkMode {
    /// `/fork` — the source's log, copied and scrubbed.
    Copy,
    /// `/summary-n-fork` — a summary of the source, produced out of band.
    Summary,
}

impl ForkMode {
    /// The wire spelling, and what a lifecycle entry carries.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Summary => "summary",
        }
    }
}

/// One fork.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkRecord {
    pub parent: ForkParent,
    /// The source agent's log seq this fork was taken at — the branch point.
    pub source_seq: u64,
    pub mode: ForkMode,
    /// What the fork has named itself, once it has. `None` until then; a client
    /// falls back to the mode and the moment.
    pub title: Option<String>,
    pub status: AgentStatus,
    pub created_at_ms: u64,
}

/// Every fork a session holds, keyed by agent id. Iteration is uuid order,
/// which is stable — the client sorts for display.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForkRoster {
    forks: BTreeMap<Uuid, ForkRecord>,
}

impl ForkRoster {
    pub fn apply_created(
        &mut self,
        id: Uuid,
        parent: ForkParent,
        source_seq: u64,
        mode: ForkMode,
        at_ms: u64,
    ) {
        self.forks.insert(
            id,
            ForkRecord {
                parent,
                source_seq,
                mode,
                title: None,
                // Nothing may run until the seed lands. The same status the
                // session uses for the same reason, and the reason a fork left
                // in it by a dead process is safe to re-seed: no turn has run.
                status: AgentStatus::Provisioning,
                created_at_ms: at_ms,
            },
        );
    }

    /// The seed is durable, so the fork may run.
    pub fn apply_seeded(&mut self, id: Uuid) {
        if let Some(rec) = self.forks.get_mut(&id) {
            rec.status = AgentStatus::Idle;
        }
    }

    pub fn apply_titled(&mut self, id: Uuid, title: String) {
        if let Some(rec) = self.forks.get_mut(&id) {
            rec.title = Some(title);
        }
    }

    pub fn apply_status(&mut self, id: Uuid, status: AgentStatus) {
        if let Some(rec) = self.forks.get_mut(&id) {
            rec.status = status;
        }
    }

    pub fn apply_deleted(&mut self, id: Uuid) {
        self.forks.remove(&id);
    }

    #[must_use]
    pub fn get(&self, id: Uuid) -> Option<&ForkRecord> {
        self.forks.get(&id)
    }

    #[must_use]
    pub fn contains(&self, id: Uuid) -> bool {
        self.forks.contains_key(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Uuid, &ForkRecord)> {
        self.forks.iter()
    }

    /// Forks whose seed never landed. Re-seeded at load: the seeding task is
    /// session-owned work with no journal of its own, so nothing else can
    /// finish one a dead process abandoned.
    #[must_use]
    pub fn seeding(&self) -> Vec<Uuid> {
        self.forks
            .iter()
            .filter(|(_, r)| matches!(r.status, AgentStatus::Provisioning))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether any fork is mid-seed, so the session must not unload out from
    /// under an in-flight summariser call.
    #[must_use]
    pub fn has_seeding(&self) -> bool {
        self.forks
            .values()
            .any(|r| matches!(r.status, AgentStatus::Provisioning))
    }
}
```

Add to `crates/server/src/sessions/mod.rs` beside the other `pub mod` lines:

```rust
pub mod forks;
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p horsie-server --lib forks::`
Expected: PASS, 8 tests

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/sessions/forks.rs crates/server/src/sessions/mod.rs
git commit -m "feat(sessions): the fork roster"
```

---

### Task 2: Fork commands, events, and state

**Files:**
- Modify: `crates/server/src/sessions/session_actor/types.rs`
- Modify: `crates/server/src/sessions/session_actor/mod.rs:930` (`apply_event` dispatch)

**Interfaces:**
- Consumes: `ForkRoster`, `ForkParent`, `ForkMode` from Task 1.
- Produces: `SessionCommand::Fork(ForkCommand)`, `ForkCommand::{Create, FinishCreate, Seeded, SeedFailed, SetTitle, Delete, ReseedInterrupted}`, `SessionDomainEvent::{ForkCreated, ForkSeeded, ForkTitled, ForkStatusChanged, ForkDeleted}`, `SessionState.forks`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/server/src/sessions/session_actor/types.rs, in its tests module
#[test]
fn a_session_state_without_forks_deserializes_empty() {
    // Every session journaled before forks existed carries no `forks` key. It
    // must load with an empty roster rather than failing `recover()` and
    // taking the whole supervisor down with it.
    let row = r#"{"status":"Idle","last_error":null}"#;
    let state: SessionState = serde_json::from_str(row).unwrap();
    assert_eq!(state.forks.iter().count(), 0);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib a_session_state_without_forks`
Expected: FAIL — no field `forks` on `SessionState`

- [ ] **Step 3: Write the implementation**

In `types.rs`, add to `SessionCommand`:

```rust
    /// The session's forks: branching a conversation, and what became of each.
    Fork(ForkCommand),
```

Add the command enum after `SubAgentCommand`:

```rust
/// Branching a conversation into a second one inside this session.
#[derive(Serialize, Deserialize)]
pub enum ForkCommand {
    /// `/fork` or `/summary-n-fork`: branch `parent` and queue `message` in the
    /// new fork. Replies with the fork's id, which is what the client redirects
    /// to.
    Create {
        parent: ForkParent,
        mode: ForkMode,
        message: String,
        reply: ReplyTo<Result<Uuid, String>>,
    },
    /// Internal: the `ForkCreated` write came back — only now does the fork's
    /// actor exist (persist-then-spawn, as a subagent spawn does). A failed
    /// write spawns nothing and the caller gets the error.
    FinishCreate {
        id: Uuid,
        parent: ForkParent,
        mode: ForkMode,
        message: String,
        reply: ReplyTo<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
    /// Internal: the detached seeding task wrote the fork's initial state.
    Seeded { id: Uuid },
    /// Internal: the detached seeding task could not. Carries the reason
    /// verbatim, because it is what the user is shown.
    SeedFailed { id: Uuid, error: String },
    /// A fork's own `set_session_title` call. Renames the fork, never the
    /// session — the model should not have to know which it is in.
    SetTitle {
        id: Uuid,
        title: String,
        reply: ReplyTo<Result<String, String>>,
    },
    /// Someone asked for this fork to go. Nothing removes one on its own.
    Delete {
        id: Uuid,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Internal: recovery found forks a dead process abandoned mid-seed.
    ReseedInterrupted,
}
```

Add to `SessionDomainEvent`:

```rust
    /// A conversation was branched. The fork's own log does not exist yet —
    /// this is the session recording that it should.
    ForkCreated {
        at_ms: u64,
        id: Uuid,
        parent: ForkParent,
        /// The source agent's log seq at the branch point.
        source_seq: u64,
        mode: ForkMode,
    },
    /// The fork's initial state is durable, so it may run.
    ForkSeeded { at_ms: u64, id: Uuid },
    /// A fork named itself.
    ForkTitled {
        at_ms: u64,
        id: Uuid,
        name: String,
    },
    /// A fork moved. Journaled so the session list is answerable without
    /// loading the session, exactly as the session's own status is.
    ForkStatusChanged {
        at_ms: u64,
        id: Uuid,
        status: AgentStatus,
    },
    /// A fork was removed, on request. Never automatic.
    ForkDeleted { at_ms: u64, id: Uuid },
```

Add to `SessionState`:

```rust
    /// Every fork this session holds. `#[serde(default)]` so pre-fork journal
    /// rows load with an empty roster.
    #[serde(default)]
    pub forks: crate::sessions::forks::ForkRoster,
```

Add the imports `types.rs` needs:

```rust
use crate::sessions::forks::{ForkMode, ForkParent, ForkRoster};
```

In `mod.rs`, extend the `apply_event` match — a new event that reaches no arm
fails to compile here, which is where classification belongs:

```rust
            SessionDomainEvent::ForkCreated { .. }
            | SessionDomainEvent::ForkSeeded { .. }
            | SessionDomainEvent::ForkTitled { .. }
            | SessionDomainEvent::ForkStatusChanged { .. }
            | SessionDomainEvent::ForkDeleted { .. } => ForkedAgents::apply(&mut state, &event),
```

and to `handle_command`:

```rust
            SessionCommand::Fork(c) => ForkedAgents::handle(self, state, c, ctx).await,
```

Both reference `ForkedAgents`, which Task 3 creates. Until then this task does
not compile on its own — implement Tasks 2 and 3 together and commit once.

- [ ] **Step 4: Deferred**

This task's test passes only once Task 3 lands. Proceed to Task 3, then run:

Run: `cargo test -p horsie-server --lib a_session_state_without_forks`
Expected: PASS

---

### Task 3: The `ForkedAgents` component

**Files:**
- Create: `crates/server/src/sessions/session_actor/fork.rs`
- Modify: `crates/server/src/sessions/session_actor/mod.rs` (`mod fork;`, `use fork::ForkedAgents;`)
- Modify: `crates/server/src/sessions/session_actor/context.rs` (`SessionAgentKind::Fork`)

**Interfaces:**
- Consumes: `ForkCommand`, `SessionDomainEvent::Fork*`, `SessionState.forks` from Task 2; `AgentPlan`, `spawn_agent`, `ResidentAgent` from `mod.rs`.
- Produces: `ForkedAgents` (with `handle` and the four `Component` hooks), `SessionActor::spawn_fork_actor`.

- [ ] **Step 1: Write the failing tests**

```rust
// at the bottom of crates/server/src/sessions/session_actor/fork.rs
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::forks::{ForkMode, ForkParent};

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn state_with_fork(status: AgentStatus) -> SessionState {
        let mut state = SessionState::default();
        state
            .forks
            .apply_created(id(1), ForkParent::Main, 0, ForkMode::Summary, 1);
        if !matches!(status, AgentStatus::Provisioning) {
            state.forks.apply_status(id(1), status);
        }
        state
    }

    /// A summariser call in flight must not be unloaded out from under itself.
    #[test]
    fn a_fork_mid_seed_keeps_the_session_loaded() {
        assert!(ForkedAgents::busy(&state_with_fork(AgentStatus::Provisioning)));
        assert!(!ForkedAgents::busy(&state_with_fork(AgentStatus::Idle)));
        assert!(!ForkedAgents::busy(&SessionState::default()));
    }

    /// Seeding is session-owned work with no journal of its own, so nothing
    /// else can finish one a dead process abandoned.
    #[test]
    fn a_fork_left_mid_seed_is_reseeded_at_load() {
        let cx = ActionCx {
            id: id(9),
            spec: &crate::sessions::spec::SessionSpec::for_vendor("mock"),
        };
        assert!(matches!(
            ForkedAgents::on_load(&cx, &state_with_fork(AgentStatus::Provisioning)),
            Some(SessionCommand::Fork(ForkCommand::ReseedInterrupted))
        ));
        assert!(
            ForkedAgents::on_load(&cx, &state_with_fork(AgentStatus::Idle)).is_none(),
            "a seeded fork has nothing to repair"
        );
    }

    #[test]
    fn the_fold_tracks_a_fork_through_its_life() {
        let mut state = SessionState::default();
        ForkedAgents::apply(
            &mut state,
            &SessionDomainEvent::ForkCreated {
                at_ms: 1,
                id: id(1),
                parent: ForkParent::Main,
                source_seq: 12,
                mode: ForkMode::Copy,
            },
        );
        assert_eq!(
            state.forks.get(id(1)).unwrap().status,
            AgentStatus::Provisioning
        );
        ForkedAgents::apply(&mut state, &SessionDomainEvent::ForkSeeded { at_ms: 2, id: id(1) });
        assert_eq!(state.forks.get(id(1)).unwrap().status, AgentStatus::Idle);
        ForkedAgents::apply(
            &mut state,
            &SessionDomainEvent::ForkTitled {
                at_ms: 3,
                id: id(1),
                name: "Other migration".to_string(),
            },
        );
        assert_eq!(
            state.forks.get(id(1)).unwrap().title.as_deref(),
            Some("Other migration")
        );
        ForkedAgents::apply(&mut state, &SessionDomainEvent::ForkDeleted { at_ms: 4, id: id(1) });
        assert!(!state.forks.contains(id(1)));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib fork::tests`
Expected: FAIL — `could not find 'fork'`

- [ ] **Step 3: Write the implementation**

Create `crates/server/src/sessions/session_actor/fork.rs`:

```rust
//! The session's forks: branching a conversation into a second one that a
//! person can talk to.
//!
//! A fork is not a subagent. It owes nobody a result, it has `ask_user`, and it
//! names itself — so it gets the main agent's toolbox layers and its own
//! roster. It is not a session either: it shares the one runtime the session
//! owns, under its own agent id, which is what makes it cheap.
//!
//! Persists a create *before* the fork's actor exists, exactly as a subagent
//! spawn does: a crash between the two replays as a fork still `Provisioning`,
//! which `on_load` re-seeds — strictly better than an untracked agent.

use super::component::{ActionCx, Component};
use super::context::SessionAgentKind;
use super::{
    AgentAction, AgentKey, AgentPlan, AgentStatus, CommandEffect, ForkCommand, SessionActor,
    SessionCommand, SessionDomainEvent, SessionState,
};
use crate::agent_loop::AgentCommand;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::forks::{ForkMode, ForkParent};
use horsie_actor::{ActorContext, ActorRef, ReplyTo};
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

pub(super) struct ForkedAgents;

impl ForkedAgents {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: ForkCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            ForkCommand::Create {
                parent,
                mode,
                message,
                reply,
            } => {
                // The branch point, read before anything is written: the seq
                // the source's log is at right now is what this fork carries.
                let Some(source_seq) = actor.agent_log_head(state, parent).await else {
                    let _ = reply.send(Err("the agent to fork is not available".to_string()));
                    return CommandEffect::none();
                };
                let id = Uuid::new_v4();
                let created = SessionDomainEvent::ForkCreated {
                    at_ms: now_ms(),
                    id,
                    parent,
                    source_seq,
                    mode,
                };
                // Persist first, spawn second — see the module doc.
                let (tx, rx) = oneshot::channel();
                let self_ref = actor.me(ctx);
                tokio::spawn(async move {
                    let persisted = rx.await.unwrap_or_else(|_| {
                        Err(horsie_actor::JournalError::Backend(
                            "fork ack channel closed".to_string(),
                        ))
                    });
                    let _ = self_ref
                        .tell(SessionCommand::Fork(ForkCommand::FinishCreate {
                            id,
                            parent,
                            mode,
                            message,
                            reply,
                            persisted,
                        }))
                        .await;
                });
                CommandEffect::persist(vec![created]).and_ack(ReplyTo::from_sender(tx))
            }
            ForkCommand::FinishCreate {
                id,
                parent,
                mode,
                message,
                reply,
                persisted,
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist fork: {e}")));
                    return CommandEffect::none();
                }
                // The message waits in the fork's own queue while the seed is
                // built, exactly as a session's first message waits behind its
                // create. The fork is `Provisioning`, so nothing drains it.
                let Some(agent) = actor.spawn_fork_actor(ctx, state, id) else {
                    let _ = reply.send(Err("could not start the fork".to_string()));
                    return CommandEffect::none();
                };
                let _ = agent
                    .tell(AgentCommand::Enqueue {
                        item: crate::agent_loop::Incoming::User {
                            id: format!("fork:{id}"),
                            text: message,
                        },
                        ack: None,
                    })
                    .await;
                actor.start_seeding(ctx, state, id, parent, mode);
                // The id travels now, not when the seed lands: the client
                // redirects to a fork that is visibly building itself, which is
                // the same thing a new session does.
                let _ = reply.send(Ok(id));
                CommandEffect::none()
            }
            ForkCommand::Seeded { id } => {
                if !state.forks.contains(id) {
                    return CommandEffect::none();
                }
                actor
                    .persist_and_advance(
                        state,
                        vec![SessionDomainEvent::ForkSeeded {
                            at_ms: now_ms(),
                            id,
                        }],
                        ctx,
                    )
                    .await
            }
            ForkCommand::SeedFailed { id, error } => {
                if !state.forks.contains(id) {
                    return CommandEffect::none();
                }
                tracing::warn!(fork = %id, error, "seeding a fork failed");
                CommandEffect::persist(vec![SessionDomainEvent::ForkStatusChanged {
                    at_ms: now_ms(),
                    id,
                    status: AgentStatus::Failed,
                }])
            }
            ForkCommand::SetTitle { id, title, reply } => {
                let normalized =
                    match crate::sessions::title_tool::normalize_session_title(&title) {
                        Ok(t) => t,
                        Err(e) => {
                            let _ = reply.send(Err(e.to_string()));
                            return CommandEffect::none();
                        }
                    };
                if !state.forks.contains(id) {
                    let _ = reply.send(Err(format!("no such fork: {id}")));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(normalized.clone()));
                CommandEffect::persist(vec![SessionDomainEvent::ForkTitled {
                    at_ms: now_ms(),
                    id,
                    name: normalized,
                }])
            }
            ForkCommand::Delete { id, reply } => {
                if !state.forks.contains(id) {
                    let _ = reply.send(Err(format!("no such fork: {id}")));
                    return CommandEffect::none();
                }
                actor.retire_fork_actor(id).await;
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionDomainEvent::ForkDeleted {
                    at_ms: now_ms(),
                    id,
                }])
            }
            ForkCommand::ReseedInterrupted => {
                for id in state.forks.seeding() {
                    let Some(rec) = state.forks.get(id) else {
                        continue;
                    };
                    actor.start_seeding(ctx, state, id, rec.parent, rec.mode);
                }
                CommandEffect::none()
            }
        }
    }
}

impl Component for ForkedAgents {
    /// A fork left `Provisioning` by a dead process. Nothing else can finish
    /// one: seeding is session-owned work with no journal of its own, unlike a
    /// turn, which the agent reports as interrupted from its own recovery.
    ///
    /// Safe to re-attempt for the reason `RuntimeLifecycle` gives about its own
    /// case: `Provisioning` is precisely the state in which no turn has run.
    fn on_load(_cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
        state
            .forks
            .has_seeding()
            .then_some(SessionCommand::Fork(ForkCommand::ReseedInterrupted))
    }

    /// A summariser call is minutes of provider time with nothing durable
    /// behind it. Unloading the session mid-seed loses it.
    fn busy(state: &SessionState) -> bool {
        state.forks.has_seeding()
    }

    // The fallthrough is unreachable by construction: `SessionActor::apply_event`
    // matches every variant explicitly and routes each to exactly one component,
    // so a newly added event fails to compile *there* — which is where it should
    // be classified — rather than silently reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::ForkCreated {
                id,
                parent,
                source_seq,
                mode,
                at_ms,
            } => state.forks.apply_created(id, parent, source_seq, mode, at_ms),
            SessionDomainEvent::ForkSeeded { id, .. } => state.forks.apply_seeded(id),
            SessionDomainEvent::ForkTitled { id, name, .. } => state.forks.apply_titled(id, name),
            SessionDomainEvent::ForkStatusChanged { id, status, .. } => {
                state.forks.apply_status(id, status);
            }
            SessionDomainEvent::ForkDeleted { id, .. } => state.forks.apply_deleted(id),
            other => unreachable!("ForkedAgents was handed {other:?}"),
        }
    }
}

/// Handlers that belong to this component but act on the actor's own fields —
/// the roster and the spawn helpers. An inherent `impl` in a child module sees
/// them, so moving the code needs no plumbing.
impl SessionActor {
    /// Spawn one fork's actor. Takes the main agent's plan: a fork is a
    /// conversation, so it gets `ask_user`, a title tool of its own, and the
    /// session's settings.
    pub(super) fn spawn_fork_actor(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
    ) -> Option<ActorRef<AgentCommand>> {
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::Fork(id),
                settings: self.spec().agent.clone(),
                step_output_schema: None,
                agent_type: None,
                // No handoff tool. A fork ends its turn with plain text, like
                // the main agent it was branched from.
                handoff_tool: None,
            },
        )
        .map(|resident| resident.actor)
    }
}
```

Wire it into `mod.rs`:

```rust
mod fork;
use fork::ForkedAgents;
```

Add the kind in `context.rs` — `SessionAgentKind::Fork(Uuid)` — and extend
every match over it. `agent_key`:

```rust
            Self::Fork(id) => AgentKey::Fork(*id),
```

`broadcasts` — a fork is something a person opens and watches, so it narrates
its own setup exactly as the main agent does:

```rust
        matches!(self, Self::Main | Self::Step(_) | Self::Fork(_))
```

`scoped_client` — a fork shares the sandbox and gets its own cwd/env bucket,
which is the arm subagents and steps already take:

```rust
        SessionAgentKind::Sub(id) | SessionAgentKind::Step(id) | SessionAgentKind::Fork(id) => {
            client.with_agent_id(id.to_string())
        }
```

The toolbox layering at `context.rs:733` — a fork takes the main agent's arms,
with the title layer pointed at itself:

```rust
            SessionAgentKind::Fork(id) => {
                let inner: Arc<dyn Toolbox> = Arc::new(AskUserToolbox::new(with_spawn));
                Arc::new(SessionTitleToolbox::for_fork(inner, self.session.clone(), id))
            }
```

`SessionTitleToolbox` gains the target rather than a second type: the tool's
name, schema and description are identical either way, and the model should not
have to know which kind of conversation it is in to name it.

```rust
/// What a `set_session_title` call renames.
#[derive(Clone, Copy)]
enum TitleTarget {
    Session,
    Fork(Uuid),
}

impl SessionTitleToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, session: SessionRef) -> Self {
        Self { inner, session, target: TitleTarget::Session }
    }

    /// The same tool, renaming one fork instead of the session.
    pub fn for_fork(inner: Arc<dyn Toolbox>, session: SessionRef, id: Uuid) -> Self {
        Self { inner, session, target: TitleTarget::Fork(id) }
    }
}
```

and in `execute`, the one branch:

```rust
        let title = self
            .session
            .ask(|reply| match self.target {
                TitleTarget::Session => SessionCommand::Core(CoreCommand::SetTitle {
                    title: title.to_string(),
                    reply,
                }),
                TitleTarget::Fork(id) => SessionCommand::Fork(ForkCommand::SetTitle {
                    id,
                    title: title.to_string(),
                    reply,
                }),
            })
            .await
```

The prompt suffix — a fork is told what it is, and that naming itself is how
it becomes findable:

```rust
/// Appended to a fork's system prompt. A fork is a conversation, so most of
/// what a subagent is told does not apply: it can ask the user and it owes
/// nobody a report. What it does need is to know it is one of several under one
/// session, and that the title is how a person tells them apart.
const FORK_PROMPT_SUFFIX: &str = "\n\n# Forked conversation\n\
You are a fork: a conversation branched from another one in this session, carrying its \
history up to the branch point. You share the workspace with it — what you change on disk \
is what it sees. Name yourself with set_session_title as soon as the direction is clear; \
that title is how a person tells this conversation from the one it came from.";
```

and in the suffix match:

```rust
            SessionAgentKind::Fork(_) => Some(FORK_PROMPT_SUFFIX),
```

`caller` for spawns — a fork roots its own subagent tree, as a step does:

```rust
            SessionAgentKind::Main | SessionAgentKind::Step(_) | SessionAgentKind::Fork(_) => {
                SubAgentParent::Main
            }
```

Add `AgentKey::Fork(Uuid)` in `types.rs` and extend `SessionAgents::get` and
`spawn_agent`'s two matches — a fork journals and is addressed under its own id,
exactly as a subagent is:

```rust
            AgentKey::Sub(id) | AgentKey::Step(id) | AgentKey::Fork(id) => self.sub(id),
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p horsie-server --lib fork::tests && cargo test -p horsie-server --lib a_session_state_without_forks`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/sessions/session_actor/ crates/server/src/sessions/mod.rs
git commit -m "feat(sessions): forked agents as a session component"
```

---

### Task 4: Seeding — copy and summary

**Files:**
- Modify: `crates/agentcore/src/compaction.rs` (add `Agent::summarise_all`)
- Modify: `crates/server/src/agent_loop/agent_actor.rs` (add `AgentCommand::ForkSeed`, `AgentCommand::SeedFrom`, `AgentState::scrub_for_fork`)
- Modify: `crates/server/src/sessions/session_actor/fork.rs` (`start_seeding`, `agent_log_head`, `retire_fork_actor`)

**Interfaces:**
- Consumes: `ForkCommand::{Seeded, SeedFailed}` from Task 2; `spawn_fork_actor` from Task 3.
- Produces: `Agent::summarise_all(instructions: Option<&str>) -> Result<String, AgentError>`; `AgentState::scrub_for_fork(&self) -> AgentState`; `SessionActor::start_seeding`, `SessionActor::agent_log_head`, `SessionActor::retire_fork_actor`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/server/src/agent_loop/agent_actor.rs, in its tests module
/// A fork must not inherit a pending question, an interrupted turn, or the
/// source's bill. It must inherit the conversation and the working state the
/// conversation refers to.
#[test]
fn a_scrubbed_state_carries_the_conversation_and_nothing_in_flight() {
    let mut source = AgentState::default();
    source.log.push(AgentLogEntry {
        seq: 7,
        at_ms: 1,
        body: AgentLogBody::Llm(Message {
            id: "m1".into(),
            role: Role::User,
            parts: vec![],
            created_at_ms: 1,
            started_at_ms: None,
        }),
    });
    source.next_seq = 8;
    source.context_tokens = 4_242;
    source.inbox.push(crate::agent_loop::Incoming::User {
        id: "queued".into(),
        text: "later".into(),
    });
    source.asks.push(crate::agent_loop::AskedQuestion {
        // `Option<String>` — `None` only for a pre-#62 journal.
        tool_call_id: Some("tc1".into()),
        question: "which one?".into(),
    });
    source.parked = true;
    source.turn_in_flight = true;
    source.usage_total = UsageTotal {
        input_tokens: 100,
        output_tokens: 200,
        cache_creation_tokens: Some(1),
        cache_read_tokens: Some(2),
    };
    source.last_turn_usage = Some(Usage {
        input_tokens: 1,
        output_tokens: 2,
        cache_creation_tokens: None,
        cache_read_tokens: None,
    });

    let fork = source.scrub_for_fork();

    assert_eq!(fork.log.len(), 1, "the conversation is the point");
    assert_eq!(fork.next_seq, 8, "numbering continues, so cursors resolve");
    assert_eq!(fork.context_tokens, 4_242, "the prompt really is that big");
    assert!(fork.inbox.is_empty());
    assert!(fork.asks.is_empty());
    assert!(fork.timers.is_empty());
    assert!(!fork.parked);
    assert!(!fork.turn_in_flight, "a fork must not start life interrupted");
    assert_eq!(
        fork.usage_total,
        UsageTotal::default(),
        "the source's spend must not be counted twice"
    );
    assert_eq!(fork.last_turn_usage, None);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib a_scrubbed_state_carries`
Expected: FAIL — no method `scrub_for_fork`

- [ ] **Step 3: Write the implementation**

On `AgentState` in `agent_actor.rs`:

```rust
    /// This conversation as a fork's starting point.
    ///
    /// Everything that is *about the conversation* carries; everything that is
    /// in flight or is a bill does not. A fork that inherited an ask would park
    /// on a question nobody asked it; one that inherited `turn_in_flight` would
    /// be reported interrupted before it had run; one that inherited
    /// `usage_total` would make the session's aggregate count the same tokens
    /// twice.
    #[must_use]
    pub fn scrub_for_fork(&self) -> Self {
        Self {
            log: self.log.clone(),
            next_seq: self.next_seq,
            context_tokens: self.context_tokens,
            task_list: self.task_list.clone(),
            inbox: Vec::new(),
            asks: Vec::new(),
            timers: Vec::new(),
            parked: false,
            turn_in_flight: false,
            usage_total: UsageTotal::default(),
            last_turn_usage: None,
        }
    }
```

Two new `AgentCommand` variants:

```rust
    /// Hand back this agent's state as a fork's starting point, and the log seq
    /// it was taken at. Read-only.
    ForkSeed {
        reply: ReplyTo<(AgentState, u64)>,
    },
    /// Adopt `state` as this agent's whole history, and append `seed` as its
    /// last entry. Sent once, to a fork, before it has run anything.
    SeedFrom {
        state: Box<AgentState>,
        seed: Box<Message>,
        reply: ReplyTo<Result<(), String>>,
    },
```

`SeedFrom` persists one `Seeded` domain event carrying the state and the seed
message, so the fork's own journal explains its history rather than a snapshot
appearing from nowhere. Fold it by replacing state wholesale and appending the
seed at `next_seq`.

In `compaction.rs`, the out-of-band summariser:

```rust
    /// Summarise this agent's whole history, changing nothing.
    ///
    /// What `/summary-n-fork` runs. Distinct from [`Self::compact`] in the one
    /// way that matters: no boundary is written and `self.history` is untouched,
    /// because the conversation being summarised is not the one that receives
    /// the summary. Folding it back would make the command do two things, only
    /// one of which was asked for.
    ///
    /// # Errors
    /// Whatever the summarising provider call fails with.
    pub async fn summarise_all(
        &mut self,
        instructions: Option<&str>,
    ) -> Result<String, AgentError> {
        self.summarise(self.history.len(), instructions).await
    }
```

In `fork.rs`, the three actor helpers:

```rust
    /// The log seq the agent behind `parent` is at — a fork's branch point.
    pub(super) async fn agent_log_head(
        &mut self,
        state: &SessionState,
        parent: ForkParent,
    ) -> Option<u64> {
        let agent = self.fork_source(state, parent)?;
        agent.ask(|reply| AgentCommand::ForkSeed { reply }).await.ok().map(|(_, seq)| seq)
    }

    /// Build and deliver a fork's initial state, off the mailbox.
    ///
    /// Detached because a `Summary` seed is a provider call: holding the
    /// session's mailbox for it would stall every other agent in the session.
    /// `ForkedAgents::busy` is what keeps the session loaded meanwhile.
    pub(super) fn start_seeding(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        parent: ForkParent,
        mode: ForkMode,
    ) {
        let (Some(source), Some(fork)) = (
            self.fork_source(state, parent),
            self.agents.as_ref().and_then(|a| a.sub(id)).map(|r| r.actor.clone()),
        ) else {
            return;
        };
        let source_title = self.spec().name.clone().unwrap_or_else(|| "this session".into());
        let self_ref = self.me(ctx);
        tokio::spawn(async move {
            let outcome = seed_fork(&source, &fork, mode, &source_title).await;
            let cmd = match outcome {
                Ok(()) => ForkCommand::Seeded { id },
                Err(error) => ForkCommand::SeedFailed { id, error },
            };
            let _ = self_ref.tell(SessionCommand::Fork(cmd)).await;
        });
    }

    /// Stop a fork's actor, if it is resident, and forget it.
    ///
    /// Best effort: a fork that is not resident has nothing to stop, and the
    /// `ForkDeleted` that follows is what makes the removal durable either way.
    pub(super) async fn retire_fork_actor(&mut self, id: Uuid) {
        let Some(agent) = self.agents.as_mut().and_then(|a| a.remove_sub(id)) else {
            return;
        };
        let _ = agent.actor.tell(AgentCommand::Shutdown).await;
    }
```

and the free function that does the work:

```rust
/// Build a fork's history from its source and hand it over.
///
/// Both modes end with one synthetic `Role::User` message carrying a `fork:`
/// id — the device compaction already uses for `compaction:{n}`, so
/// `prompt_messages` needs no change and the client special-cases an id prefix
/// it already special-cases.
async fn seed_fork(
    source: &ActorRef<AgentCommand>,
    fork: &ActorRef<AgentCommand>,
    mode: ForkMode,
    source_title: &str,
) -> Result<(), String> {
    let (state, _) = source
        .ask(|reply| AgentCommand::ForkSeed { reply })
        .await
        .map_err(|e| format!("read the source conversation: {e}"))?;
    let (state, body) = match mode {
        ForkMode::Copy => (state.scrub_for_fork(), String::new()),
        ForkMode::Summary => {
            let summary = source
                .ask(|reply| AgentCommand::SummariseAll { reply })
                .await
                .map_err(|e| format!("summarise the source conversation: {e}"))?
                .map_err(|e| format!("summarise the source conversation: {e}"))?;
            (AgentState::default(), summary)
        }
    };
    let seed = Message {
        id: format!("fork:{}", uuid::Uuid::new_v4()),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: fork_seed_text(source_title, &body),
        })],
        created_at_ms: now_ms(),
        started_at_ms: None,
    };
    fork.ask(|reply| AgentCommand::SeedFrom {
        state: Box::new(state),
        seed: Box::new(seed),
        reply,
    })
    .await
    .map_err(|e| format!("seed the fork: {e}"))?
}

/// What a fork reads first. The title instruction rides here rather than in the
/// system prompt: a prompt section is re-sent every turn and would go on
/// nagging long after the fork was named.
fn fork_seed_text(source_title: &str, summary: &str) -> String {
    let mut out = format!(
        "This conversation was forked from \"{source_title}\". The message that \
         follows sets a new direction — call set_session_title once it is clear."
    );
    if !summary.is_empty() {
        out.push_str("\n\n# Summary of the conversation this was forked from\n\n");
        out.push_str(summary);
    }
    out
}
```

Add `AgentCommand::SummariseAll { reply: ReplyTo<Result<String, String>> }` to
`agent_actor.rs`, handled by building the agent's `Agent` the way a turn does
and calling `summarise_all(None)`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p horsie-server --lib scrub_for_fork && cargo test -p horsie-agentcore --lib summarise`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/agentcore/src crates/server/src
git commit -m "feat(sessions): seed a fork by copy or by summary"
```

---

### Task 5: The builtins and the `Turns` hand-off

**Files:**
- Modify: `crates/support/src/plugin/builtins.rs`
- Modify: `crates/server/src/sessions/session_actor/turns.rs` (the builtin match at ~line 239)
- Modify: `crates/server/src/sessions/session_actor/mod.rs` (`resolve_agent`)

**Interfaces:**
- Consumes: `ForkCommand::Create`, `ForkParent`, `ForkMode` from Tasks 2–3.
- Produces: `/fork` and `/summary-n-fork` resolved into `SessionCommand::Fork(ForkCommand::Create)`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/support/src/plugin/builtins.rs, in its tests module
#[test]
fn fork_and_summary_n_fork_are_builtins_that_take_a_message() {
    for name in ["fork", "summary-n-fork"] {
        let b = builtin(name).unwrap_or_else(|| panic!("{name} is a builtin"));
        assert_eq!(b.name, name);
        assert!(
            b.argument_hint.is_some(),
            "{name} needs a message; the typeahead must say so"
        );
    }
    // Prefix matching would make `/fork` ambiguous with the longer name.
    assert!(builtin("summary-n").is_none());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-support --lib builtins`
Expected: FAIL — `fork is a builtin` panics

- [ ] **Step 3: Write the implementation**

```rust
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "compact",
        description: "Summarise earlier history to free up context. The full \
                      transcript stays readable.",
        argument_hint: Some("[what to keep]"),
    },
    Builtin {
        name: "fork",
        description: "Continue in a new conversation with a copy of this one. \
                      Same workspace, separate history.",
        argument_hint: Some("<what to do next>"),
    },
    Builtin {
        name: "summary-n-fork",
        description: "Continue in a new conversation seeded with a summary of \
                      this one. Same workspace, fresh context.",
        argument_hint: Some("<what to do next>"),
    },
];
```

In `turns.rs`, extend the builtin match. A fork is not enqueued to the agent —
it is a session-level act, so the arm returns early rather than producing an
`Incoming`:

```rust
    Some((builtin, args)) if matches!(builtin.name, "fork" | "summary-n-fork") => {
        let message = args.trim();
        if message.is_empty() {
            let _ = reply.send(Err(UserMessageError::Rejected(format!(
                "/{} needs a message saying what the new conversation should do",
                builtin.name
            ))));
            return CommandEffect::none();
        }
        // Only a conversation forks. A subagent's is delegated work and a
        // step's belongs to the run, so neither has a branch to take.
        let parent = match key {
            AgentKey::Main => ForkParent::Main,
            AgentKey::Fork(id) => ForkParent::Fork(id),
            AgentKey::Sub(_) | AgentKey::Step(_) => {
                let _ = reply.send(Err(UserMessageError::Rejected(
                    "only a conversation can be forked".to_string(),
                )));
                return CommandEffect::none();
            }
        };
        let mode = match builtin.name {
            "summary-n-fork" => ForkMode::Summary,
            _ => ForkMode::Copy,
        };
        // Handed to the component that owns `state.forks`. `Turns` recognises
        // the command; it does not write another component's slice.
        let self_ref = actor.me(ctx);
        tokio::spawn(async move {
            let created = self_ref
                .ask(|r| {
                    SessionCommand::Fork(ForkCommand::Create {
                        parent,
                        mode,
                        message,
                        reply: r,
                    })
                })
                .await;
            let answer = match created {
                Ok(Ok(id)) => Ok(id.to_string()),
                Ok(Err(why)) => Err(UserMessageError::Rejected(why)),
                Err(e) => Err(UserMessageError::Rejected(format!("fork: {e}"))),
            };
            let _ = reply.send(answer);
        });
        return CommandEffect::none();
    }
```

`resolve_agent` must resolve a fork id before it falls through to the subagent
lookup, so a message to `?aid=<fork>` reaches it and a cold fork is woken:

```rust
                if state.forks.contains(id) {
                    if let Some(agent) = self.agents.as_ref().and_then(|a| a.sub(id)) {
                        return Some((AgentKey::Fork(id), agent.actor.clone()));
                    }
                    return Some((AgentKey::Fork(id), self.spawn_fork_actor(ctx, state, id)?));
                }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p horsie-support --lib builtins && cargo build -p horsie-server`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/support/src crates/server/src
git commit -m "feat(sessions): /fork and /summary-n-fork"
```

---

### Task 6: The branch marker in the source transcript

**Files:**
- Modify: `crates/models/fluorite/agent.fl`
- Modify: `crates/server/src/sessions/lifecycle_routing.rs`
- Run: `make types`

**Interfaces:**
- Consumes: `SessionDomainEvent::ForkCreated` from Task 2.
- Produces: `LifecycleEvent::Forked(ForkLifecycle { id, title, mode })`, routed to the source agent's log.

- [ ] **Step 1: Write the failing test**

```rust
// crates/server/src/sessions/lifecycle_routing.rs, in its tests module
/// Recorded on the agent that was forked, because that is what a viewer is
/// reading when it matters — the same rule `SubAgentLifecycle` follows.
#[test]
fn a_fork_is_recorded_on_the_conversation_it_branched_from() {
    let state = SessionState::default();
    let entries = route(
        &SessionDomainEvent::ForkCreated {
            at_ms: 1,
            id: uuid::Uuid::nil(),
            parent: ForkParent::Main,
            source_seq: 4,
            mode: ForkMode::Summary,
        },
        &state,
    );
    let (key, event) = entries.into_iter().next().expect("one entry");
    assert_eq!(key, AgentKey::Main);
    match event {
        LifecycleEvent::Forked(f) => {
            assert_eq!(f.id, uuid::Uuid::nil().to_string());
            assert_eq!(f.mode, "summary");
        }
        other => panic!("expected a fork entry, got {other:?}"),
    }
}

/// A fork of a fork is recorded on that fork, not on the main agent.
#[test]
fn a_nested_fork_is_recorded_on_its_parent_fork() {
    let parent = uuid::Uuid::from_bytes([3; 16]);
    let state = SessionState::default();
    let entries = route(
        &SessionDomainEvent::ForkCreated {
            at_ms: 1,
            id: uuid::Uuid::nil(),
            parent: ForkParent::Fork(parent),
            source_seq: 0,
            mode: ForkMode::Copy,
        },
        &state,
    );
    assert_eq!(entries[0].0, AgentKey::Fork(parent));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib lifecycle_routing`
Expected: FAIL — no variant `Forked`

- [ ] **Step 3: Write the implementation**

In `crates/models/fluorite/agent.fl`, add to the `LifecycleEvent` union:

```
    Forked(ForkLifecycle),
```

and the struct beside `SubAgentLifecycle`:

```
/// A conversation branched from this one, and where it went. Recorded on the
/// agent that was forked, because that is what a viewer is reading when it
/// matters — the same rule `SubAgentLifecycle` follows.
///
/// Never reaches the model: `prompt_messages` drops every lifecycle body, which
/// is deliberate. A fork is for the person reading, and telling the source
/// about it would disturb its prompt cache for nothing.
struct ForkLifecycle { id: String, title: Option<String>, mode: String }
```

Run `make types`.

In `lifecycle_routing.rs`:

```rust
        // On the conversation that was forked, not the session-wide log: a
        // fork of a fork belongs in that fork's transcript.
        E::ForkCreated { id, parent, mode, .. } => vec![(
            match parent {
                ForkParent::Main => AgentKey::Main,
                ForkParent::Fork(p) => AgentKey::Fork(*p),
            },
            LifecycleEvent::Forked(ForkLifecycle {
                id: id.to_string(),
                title: None,
                mode: mode.as_str().to_string(),
            }),
        )],
        // Nothing in the source's transcript changes when a fork is seeded,
        // renamed, moves or goes. Its own log is where those belong.
        E::ForkSeeded { .. }
        | E::ForkTitled { .. }
        | E::ForkStatusChanged { .. }
        | E::ForkDeleted { .. } => Vec::new(),
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p horsie-server --lib lifecycle_routing`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/models crates/server/src clients/web/src/generated
git commit -m "feat(sessions): record a fork on the conversation it branched from"
```

---

### Task 7: Mirror forks into the session list

**Files:**
- Modify: `crates/server/src/sessions/supervisor.rs`
- Modify: `crates/server/src/sessions/session_actor/core.rs` (report fork rows alongside status)
- Modify: `crates/models/fluorite/session_api.fl`, then `make types`
- Modify: `crates/server/src/http/handlers.rs` (`summary`)

**Interfaces:**
- Consumes: `ForkRoster` from Task 1.
- Produces: `SessionRecord.forks: BTreeMap<Uuid, ForkRow>`, `SessionSupervisorCommand::ForksChanged`, `SessionSummary.forks: Vec<ForkView>`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/server/src/sessions/supervisor.rs, in its tests module
/// The sidebar is built from the durable registry — `List` is documented
/// "Loads nothing". Deriving fork rows from session state instead would wake
/// every session that has ever been forked, every time someone opens the app.
#[tokio::test]
async fn forks_are_listed_without_loading_the_session() {
    let f = fixture().await;
    let sup = spawn_supervisor(&f).await;
    let id = create(&sup).await;
    let fork = Uuid::from_bytes([7; 16]);

    sup.tell(SessionSupervisorCommand::ForksChanged {
        id: id.clone(),
        forks: vec![ForkRow {
            id: fork,
            parent: None,
            title: Some("Other migration".into()),
            status: AgentStatus::Idle,
            created_at_ms: 5,
        }],
    })
    .await
    .unwrap();

    // A fresh supervisor over the same journal: nothing is loaded.
    let cold = spawn_supervisor(&f).await;
    let listed = cold
        .ask(|reply| SessionSupervisorCommand::List { reply })
        .await
        .unwrap();
    let (_, rec) = listed.iter().find(|(s, _)| *s == id).expect("the session");
    assert_eq!(rec.forks.len(), 1);
    assert_eq!(
        rec.forks.get(&fork).unwrap().title.as_deref(),
        Some("Other migration")
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib forks_are_listed_without_loading`
Expected: FAIL — no variant `ForksChanged`

- [ ] **Step 3: Write the implementation**

```rust
/// One fork under a session, as the session list holds it.
///
/// A projection of `ForkRecord`, not a second source of truth — the same
/// relationship `SessionRecord.status` has to the session's own journal, and
/// durable for the same reason: `List` loads nothing, so what it cannot read
/// from the registry it cannot show at all.
///
/// `parent: None` means the session's main agent. Flattening `ForkParent` to an
/// `Option` here is what lets a client nest forks without learning the server's
/// own vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkRow {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub title: Option<String>,
    pub status: AgentStatus,
    pub created_at_ms: u64,
}
```

on `SessionRecord`:

```rust
    /// This session's forks. Field-level `#[serde(default)]` so pre-fork
    /// journal rows load with none.
    #[serde(default)]
    pub forks: BTreeMap<Uuid, ForkRow>,
```

journaled by a `SessionForksChanged { id, forks }` supervisor event, told by the
session actor from `on_events_persisted` beside `report_status`. Follow
`SessionRecord.status` exactly: journaled only when it differs from what is
already recorded, so a session that loads and re-reports what it recovered
writes nothing.

The wire view in `session_api.fl`:

```
/// One fork under a session, for the list. `parent` absent means the session's
/// main agent; present names another fork, which is what lets a client nest
/// them without knowing the server's own vocabulary.
struct ForkView {
    id: String,
    parent: Option<String>,
    title: Option<String>,
    status: String,
    createdAtMs: u64,
}
```

added to `SessionSummary` as `forks: Vec<ForkView>`. Run `make types`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p horsie-server --lib forks_are_listed_without_loading`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/models crates/server/src clients/web/src/generated
git commit -m "feat(sessions): list a session's forks without loading it"
```

---

### Task 8: HTTP — redirect id and delete

**Files:**
- Modify: `crates/models/fluorite/session_api.fl` (`SessionAck.forkedAgent`), then `make types`
- Modify: `crates/server/src/http/handlers.rs` (`send_message`, new `delete_fork`)
- Modify: `crates/server/src/http/mod.rs` (route)

**Interfaces:**
- Consumes: `ForkCommand::Delete` from Task 2; the fork id returned through `UserMessage`'s reply from Task 5.
- Produces: `SessionAck { message_id, forked_agent }`; `DELETE /api/sessions/{id}/agents/{agent_id}`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/server/src/http/mod.rs, in its tests module
#[tokio::test]
async fn forking_answers_with_the_new_agent_to_open() {
    let (app, id) = session_fixture().await;
    let res = app
        .clone()
        .oneshot(post_json(
            &format!("/api/sessions/{id}/messages"),
            &serde_json::json!({ "text": "/fork try the other migration" }),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let ack: serde_json::Value = body_json(res).await;
    // camelCase on the wire; a snake_case key here reads as absent.
    let fork = ack["forkedAgent"].as_str().expect("a fork to open");
    assert!(Uuid::parse_str(fork).is_ok());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib forking_answers_with`
Expected: FAIL — `forkedAgent` is null

- [ ] **Step 3: Write the implementation**

`UserMessage`'s reply already carries a `String`. Rather than overload it,
`TurnCommand::UserMessage`'s reply becomes
`Result<MessageAccepted, UserMessageError>` where:

```rust
/// What accepting a message produced: the message's own id, and — when the
/// message was a `/fork` — the agent the client should open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAccepted {
    pub message_id: String,
    /// The fork this message created, for the client to redirect to. Absent for
    /// every ordinary message, which is what makes the field additive.
    pub forked_agent: Option<String>,
}
```

`send_message` maps it onto `SessionAck { message_id, forked_agent }`.
`create_session` ignores `forked_agent` — a session's first message cannot be a
fork, since there is nothing yet to fork.

`delete_fork` routes `ForkCommand::Delete` and answers `204`, `404` for an id
that names no fork.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p horsie-server --lib forking_answers_with`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/models crates/server/src clients/web/src/generated
git commit -m "feat(api): answer a fork with the agent to open, and let one be deleted"
```

---

### Task 9: End-to-end

**Files:**
- Modify: `crates/tests/tests/session_server_e2e.rs`

- [ ] **Step 1: Write the failing tests**

Three tests, all waiting on **reply text**, never on a status — a session
reports `Idle` twice and the wait can return between them, which is the trap
this suite documents:

```rust
/// `/fork` in the composer, end to end.
#[tokio::test]
async fn a_fork_carries_the_conversation_and_answers_its_own_message() { /* ... */ }

/// `/summary-n-fork` starts small: the source's messages are not in the fork's
/// log, and the fork still answers.
#[tokio::test]
async fn a_summary_fork_starts_from_a_summary_rather_than_the_history() { /* ... */ }

/// The branch point is visible where it happened.
#[tokio::test]
async fn the_source_transcript_records_where_a_fork_left() { /* ... */ }
```

Use the mock-llm runtime the suite already uses; a fake runtime daemon must
answer `ScanWorkspace` (and `SessionStart` when `use_plugins` resolves true) or
provisioning hangs with no output. On macOS run with `TMPDIR=/tmp`.

- [ ] **Step 2: Run to verify they fail**

Run: `TMPDIR=/tmp cargo test -p horsie-tests --test session_server_e2e fork`
Expected: FAIL

- [ ] **Step 3–4: Implement until green, then commit**

```bash
git add crates/tests/tests/session_server_e2e.rs
git commit -m "test(sessions): forked agents end to end"
```

---

### Task 10: Web — nested sidebar and redirect

**Files:**
- Modify: `clients/web/src/components/Sidebar.tsx`, `SessionRow.tsx`
- Modify: `clients/web/src/components/Composer.tsx` or `pages/SessionView.tsx` (redirect on `forkedAgent`)
- Modify: `clients/web/src/components/Transcript.tsx` (render the `Forked` lifecycle entry)
- Test: `clients/web/src/components/Sidebar.test.tsx`, `clients/web/e2e/`

**Interfaces:**
- Consumes: `SessionSummary.forks` from Task 7, `SessionAck.forkedAgent` from Task 8, `LifecycleEvent.Forked` from Task 6.

- [ ] **Step 1: Install and write the failing test**

`bun install` — `npm ci` fails in a fresh worktree, and CI uses
`bun install --frozen-lockfile`.

```tsx
// Sidebar.test.tsx
it("nests a fork of a fork under the fork it came from", () => {
  // forks: [{id: "a", parent: null}, {id: "b", parent: "a"}]
  // expect b's row to render at depth 2 under a
});

it("badges each row with its own status, not a rollup", () => {
  // session idle + fork running -> the session row still reads idle
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd clients/web && bun run test Sidebar`
Expected: FAIL

- [ ] **Step 3: Implement**

Build the tree from the flat `forks` array by `parent`; a `parent` that
resolves to nothing renders at the top level, which is what a deleted parent
leaves behind. Navigate to `/sessions/<id>/agents/<forkedAgent>` when the ack
carries one.

- [ ] **Step 4: Verify**

Run: `cd clients/web && bun run test && bun run build`
Expected: PASS. Note `tsc --noEmit` alone is a no-op here; `build` is the real
type check.

- [ ] **Step 5: Commit**

```bash
git add clients/web
git commit -m "feat(web): nest forks under their session and open a new one"
```

---

### Task 11: Cleanup and full verification

**Files:**
- Modify: `crates/server/src/db/journal.rs` (delete `copy_snapshot`)
- Modify: `crates/server/src/sessions/session_actor/testing.rs` (its fake's impl)
- Modify: `crates/server/tests/sql_journal.rs` (delete its two tests)

- [ ] **Step 1: `copy_snapshot` stays — record why**

It is dead code built for a fork and unusable by one, but it is a *required*
method on the `Journal` trait in the published `horsie-actor` crate. Deleting it
means releasing a new actor version, and a feature must not wait on a dependency
release for a tidy-up. Leave it; note it in the spec and the PR.

- [ ] **Step 2: Full workspace verification**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # fmt before clippy
TMPDIR=/tmp cargo test --workspace
cd clients/web && bun run test && bun run build
```

Expected: all green. `-p horsie-server` alone is a false green — the e2e suite
hits the routes too.

- [ ] **Step 3: Commit and open the PR**

```bash
git add -A
git commit -m "refactor(journal): drop the unused fork snapshot copy"
git push -u origin feat/forked-agents
gh pr create --title "feat(sessions): forked agents" --body-file /tmp/fork-pr-body.md
```

Read `.github/pull_request_template.md` first and follow it. Conventional
title; `!` only if something breaks. Write the body to a file, one long line per
paragraph and per bullet — never hard-wrapped. Never `-c user.name` /
`-c user.email`; a wrong identity fails the CLA check. Do not enable
auto-merge — a green PR is the finish line.
