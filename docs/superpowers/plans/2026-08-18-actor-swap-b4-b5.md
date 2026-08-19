# Session Actor Swap (Phase B, tasks B4–B5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `runners::SessionState` the session actor's state, dispatch every command by an agent→runner lookup, and delete the four-kind vocabulary it replaces.

**Architecture:** Phase A built the runner tree as pure logic — `Runner`, `AgentLifecycle`, `SessionState::apply`, the projections in `runners/reads.rs` and `runners/lifecycle_routing.rs` — all tested against hand-built state with no actor. This plan is the wiring: the actor's `EventSourcedActor` impl folds through `SessionState::apply`, `handle_command` resolves `state.agents[id] -> runner` instead of probing three registries in a load-bearing order, and the turn boundary performs `Action`s and re-drives from `Runner::actions`. Then the six components and the three registries they read are deleted.

**Tech Stack:** Rust 2024, `horsie-actor` (event-sourced actors), `sqlx` journal, `async_trait`, `tokio`.

**Spec:** `docs/superpowers/specs/2026-08-15-session-runners-design.md`. Locked decisions, invariants and the two full flows live there; this plan argues from it and does not restate it.

**Prior plan:** `docs/superpowers/plans/2026-08-15-session-runners.md` scoped B4/B5 and explicitly deferred decomposition until Phase A's signatures were real. They are real now, and reading them changed three things, recorded under "What the real signatures changed" below.

## Global Constraints

- **Journals break deliberately, on both sides.** Existing sessions are truncated, not migrated. No `#[serde(default)]` bridges, no shims. (Spec, locked decision 3.)
- **No backward compatibility.** Go to the right end state.
- **Iterate with `cargo test -p horsie-server --lib <filter>`.** Nothing wider until ready to commit. `crates/server` is ~95k of the workspace's ~141k lines and relinks 22 integration test binaries.
- **Do not alternate clippy and tests.** `--all-features` and the default feature set are two build graphs sharing no artifacts; every switch pays a full rebuild. Run `cargo clippy --locked --all-targets --all-features -- -D warnings` once, immediately before committing.
- **`-p horsie-server --lib` is a false green** for HTTP routes, the session actor's public behaviour, and recovery. Those are only exercised by `crates/tests`. Run the relevant suite before claiming a task done.
- **Never `cargo +nightly fmt`.** `.rustfmt.toml` declares nightly-only options; CI ignores them and a nightly run reformats the tree. Stable `cargo fmt` only.
- **`-D warnings` is not optional.** CI adds it; a local clippy without it exits 0 and reddens the PR.
- **A fold may not read a clock.** `at_ms` arrives on the event. Reaching for `now_ms()` inside an `apply` is the one thing `Runner::apply`'s signature exists to prevent.

---

## What the real signatures changed

Three findings from reading Phase A as built. Each one moves work between tasks, so they are recorded before the tasks rather than inside them.

**1. B1–B3 are already satisfied, in a different form.** The prior plan specified `sessions/session_toolbox.rs` and `sessions/equipment.rs` — one forwarding wrapper plus a builder folding `layers` over a base toolbox. What shipped instead is `Capability::layer` plus `agent_loop/toolbox.rs::claiming()`: each capability wraps the toolbox for itself, and wrapping order *is* precedence. That is strictly better — the prior shape made a capability list satisfy two orderings that read opposite ways (first in offer order, outermost in the toolbox), and this collapses them to one rule. **No task below builds either file.**

**2. B4d is much smaller than scoped.** The prior plan has `lifecycle_routing.rs` as "the heaviest consumer, and the one to rewrite first". It is already written, against `SessionEvent`, with an exhaustive table and no catch-all arm at any level — `runners/lifecycle_routing.rs`. The same is true of the read projections in `runners/reads.rs`. B4d is therefore *rebinding call sites*, not rewriting logic.

**3. Nothing consumes `RunnerArgs`.** `runners/action.rs` defines `RunnerArgs::{SubAgent, Workflow, Conversation}` and every capability produces one, but no code turns one into a `RunnerState`. That conversion is a real gap and it is the first task below, because it is pure, independently testable, and everything else needs it.

---

## File Structure

**Created:**

- `crates/server/src/sessions/runners/birth.rs` — `RunnerArgs` → `RunnerState`. One pure function, no clock, no ids minted (the capability already minted them). Its own file because it is the one seam where the session's vocabulary becomes a runner's slice, and it is the thing every `CreateChild` goes through.

**Rewritten:**

- `crates/server/src/sessions/session_actor/mod.rs` — the actor: fold, dispatch, boundary, one flat agent map.
- `crates/server/src/sessions/session_actor/types.rs` — `SessionCommand` shrinks to eight variants; `AgentKey` and `SessionAgentKind` go; the `AskAnswer`/`AnswerError` re-export **stays**.
- `crates/server/src/sessions/session_actor/reads.rs` — delegates to `runners::reads`.
- `crates/server/src/sessions/session_actor/hooks.rs` — routes by `AgentId`.
- `crates/server/src/sessions/session_actor/context.rs` — `loading_for` takes an `AgentId` and a role, not an `AgentKey`.

**Deleted (B5):**

- `session_actor/{fork,subagent,turns,run,lifecycle,component}.rs` — 5,487 lines
- `sessions/{forks,subagents,lifecycle_routing,orchestrator}.rs` — 2,082 lines + orchestrator

**Edges to update:**

- `sessions/supervisor.rs` — 17 call sites across the six command groups; reply types.
- `http/handlers.rs` — the only HTTP file naming actor types: `AgentEntry`, `SessionSnapshot`, `MAIN_AGENT_ID`, `to_wire_agent`.
- `sessions/events.rs` — `fold_session_state`/`fold_agent_state` are `#[cfg(test)]` helpers used throughout the actor tests; rebind to `SessionState::apply`.
- `sessions/workflow/driver.rs` — reaches into `state.run` directly; rebind to the runner's slice.

---

### Task 1: `RunnerArgs` becomes a `RunnerState`

The gap finding 3 names. Pure, and green on its own.

**Files:**
- Create: `crates/server/src/sessions/runners/birth.rs`
- Modify: `crates/server/src/sessions/runners/mod.rs` (add `pub mod birth;`)

**Interfaces:**
- Consumes: `RunnerArgs`, `Branch`, `WorkflowSource` from `runners::action`; `conversation::State`, `subagent::State`, `workflow::State`, `runtime::State`; `Capabilities` and `assemble(kind, &Assembly)`.
- Produces: `pub fn born(args: RunnerArgs, caps: Capabilities, run: RunnerId) -> Result<RunnerState, String>` and `pub fn runtime_born() -> RunnerState`. Task 5 calls both from `Action::CreateChild`; Task 3 calls `runtime_born` when recording the spec.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::runners::action::ForkMode;

    fn settings() -> AgentSettings {
        crate::sessions::runners::empty_settings()
    }

    /// A worker's task is also its first input, so it is stored verbatim: a
    /// restart before the agent existed re-sends exactly what was asked for.
    #[test]
    fn a_subagent_keeps_its_task_verbatim() {
        let agent = AgentId::new_v4();
        let args = RunnerArgs::SubAgent {
            agent,
            label: "read the flake".into(),
            task: "  find why it fails  ".into(),
            agent_type: None,
            settings: Box::new(settings()),
        };
        let state = born(args, Capabilities::default(), RunnerId::new_v4()).unwrap();
        let RunnerState::SubAgent(s) = state else {
            panic!("expected a subagent")
        };
        assert_eq!(s.agent, agent);
        assert_eq!(s.task, "  find why it fails  ");
        assert_eq!(s.label, "read the flake");
        assert!(!s.started, "creation starts nothing; actions() does");
        assert!(s.result.is_none());
    }

    /// A fork is a conversation with a branch point, and the message it was
    /// created with is its first input rather than a title.
    #[test]
    fn a_fork_is_a_conversation_carrying_its_branch() {
        let agent = AgentId::new_v4();
        let source = AgentId::new_v4();
        let branch = Branch {
            source,
            source_seq: 42,
            mode: ForkMode::Copy,
        };
        let args = RunnerArgs::Conversation {
            agent,
            seed: Some(branch.clone()),
            message: "carry on from here".into(),
            settings: Box::new(settings()),
        };
        let state = born(args, Capabilities::default(), RunnerId::new_v4()).unwrap();
        let RunnerState::Conversation(s) = state else {
            panic!("expected a conversation")
        };
        assert_eq!(s.seed, Some(branch));
        assert!(!s.seeded, "a fork is not seeded until its seed lands");
        assert_eq!(s.first_message.as_deref(), Some("carry on from here"));
        assert!(s.title.is_none(), "a fork names itself later, or not at all");
    }

    /// The session's own conversation: no branch, and nothing waiting to be
    /// seeded, so `actions()` may start it as soon as the runtime is ready.
    #[test]
    fn the_root_conversation_has_no_branch_and_is_already_seeded() {
        let args = RunnerArgs::Conversation {
            agent: AgentId::new_v4(),
            seed: None,
            message: String::new(),
            settings: Box::new(settings()),
        };
        let state = born(args, Capabilities::default(), RunnerId::new_v4()).unwrap();
        let RunnerState::Conversation(s) = state else {
            panic!("expected a conversation")
        };
        assert!(s.seed.is_none());
        assert!(s.seeded, "nothing has to land before the session's own agent runs");
        assert_eq!(s.first_message, None, "an empty create message is no message");
    }

    /// The graph is journal data on the runner, which is what makes an ad-hoc
    /// run — a graph with no definition row and no name — expressible.
    #[test]
    fn a_workflow_snapshots_the_graph_it_was_given() {
        let run = RunnerId::new_v4();
        let graph = Arc::new(crate::sessions::workflow::WorkflowRunSpec::default());
        let args = RunnerArgs::Workflow {
            source: WorkflowSource::Graph(Arc::clone(&graph)),
            input: "go".into(),
        };
        let state = born(args, Capabilities::default(), run).unwrap();
        let RunnerState::Workflow(s) = state else {
            panic!("expected a workflow")
        };
        assert_eq!(s.run, run, "a run's slice names the runner it is");
        assert!(Arc::ptr_eq(&s.graph, &graph));
        assert!(s.steps.is_empty(), "no step has run yet");
    }

    /// A name is not a graph. Resolving one is a database read, and a database
    /// read may not happen on the session mailbox — so this is refused here and
    /// the session resolves it before calling.
    #[test]
    fn a_named_workflow_is_refused_because_resolving_is_not_this_function() {
        let args = RunnerArgs::Workflow {
            source: WorkflowSource::Named("nightly".into()),
            input: "go".into(),
        };
        let err = born(args, Capabilities::default(), RunnerId::new_v4()).unwrap_err();
        assert!(err.contains("nightly"), "the error names what was unresolved: {err}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server --lib runners::birth`
Expected: FAIL — `could not find birth in runners`.

- [ ] **Step 3: Write the implementation**

```rust
//! Where a runner's slice comes from: one `RunnerArgs`, one `RunnerState`.
//!
//! The one seam at which the session's vocabulary becomes a runner's own, and
//! a function rather than a method on either side — `RunnerArgs` is the
//! capability's word for what it wants, `RunnerState` is the journal's word for
//! what exists, and neither should know the other's shape.
//!
//! Nothing here mints an id or reads a clock. The capability that asked already
//! minted the ids — that is what lets `spawn_agent` answer the model before the
//! child has been equipped — and the timestamp is a fact about the journal
//! entry, stamped by the session when it persists.

use super::action::{RunnerArgs, WorkflowSource};
use super::ids::{AgentId, RunnerId};
use super::{RunnerState, conversation, runtime, subagent, workflow};
use crate::agent_loop::capabilities::Capabilities;

/// The slice a freshly created runner starts life with.
///
/// `Err` for the one case that cannot be answered here: a workflow named rather
/// than given. Turning a name into a graph is a database read, and a database
/// read may not happen on the session mailbox, so the session resolves it on a
/// detached task and calls again with [`WorkflowSource::Graph`].
pub fn born(
    args: RunnerArgs,
    capabilities: Capabilities,
    run: RunnerId,
) -> Result<RunnerState, String> {
    Ok(match args {
        RunnerArgs::SubAgent {
            agent,
            label,
            task,
            agent_type,
            settings,
        } => RunnerState::SubAgent(subagent::State {
            agent,
            started: false,
            label,
            task,
            agent_type,
            settings: *settings,
            usage: crate::agent_loop::UsageTotal::default(),
            result: None,
            capabilities,
        }),
        RunnerArgs::Conversation {
            agent,
            seed,
            message,
            settings,
        } => RunnerState::Conversation(conversation::State {
            agent,
            // A fork waits for its branch to land; the session's own
            // conversation has nothing to wait for and is seeded by
            // construction. One field, so `actions()` needs no second question.
            seeded: seed.is_none(),
            seed,
            started: false,
            turn: conversation::TurnStatus::default(),
            title: None,
            first_message: (!message.is_empty()).then_some(message),
            settings: *settings,
            usage: crate::agent_loop::UsageTotal::default(),
            last_error: None,
            last_activity_ms: 0,
            capabilities,
        }),
        RunnerArgs::Workflow { source, input } => match source {
            WorkflowSource::Named(name) => {
                return Err(format!(
                    "workflow {name} has to be resolved to a graph before its runner is created"
                ));
            }
            WorkflowSource::Graph(graph) => RunnerState::Workflow(Box::new(workflow::State {
                run,
                graph,
                steps: Vec::new(),
                status: workflow::WorkflowRunStatus::default(),
                output: None,
                error: None,
                usage: crate::agent_loop::UsageTotal::default(),
                step_usage: std::collections::BTreeMap::new(),
                capabilities,
            })),
        },
    })
}

/// The sandbox's slice. Not born from args, because nobody's agent asks for it:
/// it is created with the session, from the session's own spec.
#[must_use]
pub fn runtime_born() -> RunnerState {
    RunnerState::Runtime(runtime::State::default())
}
```

Add to `crates/server/src/sessions/runners/mod.rs`, in the module list, alphabetically after `action`:

```rust
pub mod birth;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-server --lib runners::birth`
Expected: PASS, 5 tests. If `workflow::State.status`, `conversation::TurnStatus` or `WorkflowRunStatus` do not have `Default`, derive it rather than picking a literal here — a start state is the type's business.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt
cargo clippy --locked --all-targets --all-features -- -D warnings
git add crates/server/src/sessions/runners/birth.rs crates/server/src/sessions/runners/mod.rs
git commit -m "feat(sessions): one seam where a runner's arguments become its slice"
```

---

### Task 2: `AgentRole`, the four decisions that are not identity

`AgentKey` carries two questions at once: *who is this agent* and *how do I treat it*. The first becomes a lookup — `state.agents[&id]`. This task extracts the second into `AgentRole`, which is what the flat map in Task 3 needs in order to spawn an agent without a key.

**Corrected during execution — the flat map moved to Task 3, and `AgentKey` dies in Task 8.** The first draft of this task deleted `AgentKey` here, on the reasoning that doing it while the old components still compile means the compiler checks each call site. Measuring first showed that backwards: of 142 uses, **60 are in files Task 8 deletes** — `sessions/lifecycle_routing.rs` alone has 31, `turns.rs` 16, `orchestrator.rs` 7. Rewriting those to `AgentId` is work discarded an hour later, and worse, it is the kind of mechanical edit that quietly changes behaviour in code nobody will review because it is about to be deleted. `AgentKey` dies *with* its users, not before them.

So this task adds a type and deletes nothing.

**Files:**
- Modify: `crates/server/src/sessions/runners/loading.rs` (add `AgentRole`)

**Interfaces:**
- Consumes: `RunnerKind` from `runners::ids`.
- Produces: `pub enum AgentRole { Root, Fork, Sub, Step }` with `AgentRole::of(kind: RunnerKind, is_root: bool) -> Self` and `AgentRole::scoped(self) -> bool`. Task 3 calls both from `spawn_agent`.

**Why a role and not just an id.** `AgentKey` was carrying four decisions besides identity, and they do not all reduce to "look up the runner": the root conversation's runtime client stays unscoped while everything else gets `with_agent_id` (spec, swap-decision 5 — scoping the root would silently move every existing session's working directory), the subagent role prompt, `SubagentStart` vs `SessionStart`, and progress narration. So `Loading` gains `role: AgentRole { Root, Fork, Sub, Step }`, derived from the owning `RunnerKind` plus `runner == state.root`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/server/src/sessions/runners/loading.rs, in its tests module
/// The root conversation's client is unscoped and every other agent's is
/// scoped. Getting this backwards moves every existing session's working
/// directory, silently, and only on the second turn.
#[test]
fn only_the_root_conversation_is_unscoped() {
    assert!(!AgentRole::Root.scoped());
    assert!(AgentRole::Fork.scoped());
    assert!(AgentRole::Sub.scoped());
    assert!(AgentRole::Step.scoped());
}

/// A role is derived from the runner that owns the agent, never remembered:
/// a second field would be a second place for it to be wrong.
#[test]
fn a_role_comes_off_the_runner_and_whether_it_is_the_root() {
    assert_eq!(AgentRole::of(RunnerKind::Conversation, true), AgentRole::Root);
    assert_eq!(AgentRole::of(RunnerKind::Conversation, false), AgentRole::Fork);
    assert_eq!(AgentRole::of(RunnerKind::SubAgent, false), AgentRole::Sub);
    assert_eq!(AgentRole::of(RunnerKind::Workflow, false), AgentRole::Step);
    // A workflow-rooted session's steps are still steps: being the root makes
    // a conversation the session's, and makes nothing else anything.
    assert_eq!(AgentRole::of(RunnerKind::Workflow, true), AgentRole::Step);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib runners::loading`
Expected: FAIL — `cannot find type AgentRole`.

- [ ] **Step 3: Write the implementation**

In `crates/server/src/sessions/runners/loading.rs`:

```rust
/// What an agent is, for the four decisions that are not identity.
///
/// `AgentKey`'s replacement, and deliberately not the same thing: a key
/// answered "who is this" *and* "how do I treat it", and the first question is
/// now a map lookup. What is left is the second, derived from the owning runner
/// rather than stored — a stored role is a second place for it to be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// The session's own conversation. The one agent whose runtime client is
    /// unscoped, because the cwd and env bucket hang off it and scoping it now
    /// would move every existing session's working directory.
    Root,
    Fork,
    Sub,
    Step,
}

impl AgentRole {
    #[must_use]
    pub fn of(kind: RunnerKind, is_root: bool) -> Self {
        match kind {
            RunnerKind::Conversation if is_root => Self::Root,
            RunnerKind::Conversation => Self::Fork,
            RunnerKind::SubAgent => Self::Sub,
            RunnerKind::Workflow | RunnerKind::Runtime => Self::Step,
        }
    }

    /// Whether this agent's runtime client is scoped to its own id.
    #[must_use]
    pub fn scoped(self) -> bool {
        !matches!(self, Self::Root)
    }
}
```

Nothing else changes here. `Loading.key: AgentKey` stays as it is until Task 3, when the flat map arrives and there is something to replace it *with* — swapping the field now would leave `AgentRole` derivable from nothing, since the runner that decides it is not consulted until the fold lands.

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-server --lib runners::loading`
Expected: PASS, 5 tests — the 2 new ones plus the 3 already there. The tree stays green: this task only adds a type.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat(sessions): a role for the four things an agent key decided besides identity"
```

---

### Task 3: the fold, the flat agent map, and the actor's trait impl

**This is the atomic one.** There is no intermediate commit in which half the session reads `state.run` and half reads `state.runners`, and the flat agent map is part of the same change: `spawn_agent` cannot register by `AgentKey` once the runner tree decides who exists. Tasks 4–7 are all downstream of a tree that compiles again.

`SessionAgents` — the `Interactive { main, subs }` / `Workflow { live }` enum keyed by `AgentKey` — collapses to `HashMap<AgentId, ResidentAgent>` here. The topology that enum encoded (a workflow has no main agent) is now just which runner is the root.

**Files:**
- Modify: `crates/server/src/sessions/session_actor/mod.rs:99-182` (`SessionAgents` → a plain map), `:205-243` (`SessionActor.agents`), `:463-597` (`spawn_agent` takes an `AgentId` + `AgentRole`), `:867-905` (`reach`), and `:1179-1290` (the `EventSourcedActor` impl)
- Modify: `crates/server/src/sessions/session_actor/mod.rs` (`record_lifecycle`, `report_forks`, `report_status`)

**Interfaces:**
- Consumes: `SessionState::apply`, `SessionState::status`, `runners::reads::fork_rows`, `runners::lifecycle_routing::route`, `birth::runtime_born` (Task 1), `AgentRole` (Task 2).
- Produces: the actor compiles as `EventSourcedActor<Event = SessionEvent, State = runners::SessionState>`; `SessionActor.agents: HashMap<AgentId, ResidentAgent>`; `fn reach(&mut self, agent: AgentId, state: &SessionState, ctx: &ActorContext<SessionInbox>) -> Option<ActorRef<AgentCommand>>`. Tasks 4, 5 and 6 all call `reach`.

**Salvage:** reverted commit `0ef59ebb` on this branch holds a first cut of the trait impl plus `view`/`busy`/`next_actions`/`owed`, which typechecked against the runner tree. Take them from there rather than rewriting.

The flat map, and `reach` losing its four-arm match:

```rust
    /// The agent actors this session hosts, resident while this actor is
    /// loaded.
    ///
    /// One flat map, because one flat id space: which runner owns an agent is
    /// `state.agents[&id]`, and the topology the enum encoded — a workflow has
    /// no main agent — is now just which runner is the root.
    agents: HashMap<AgentId, ResidentAgent>,

    /// The mailbox of one of this session's agents, spawning a cold one on
    /// demand. `None` when no runner owns that id.
    ///
    /// The three-registry probe this replaces answered "what kind of agent is
    /// this uuid" by trying the run log, then the fork roster, then the
    /// subagent forest — an order that was load-bearing, and getting it wrong
    /// made a fork of a fork read as a fork of a subagent.
    fn reach(
        &mut self,
        agent: AgentId,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Option<ActorRef<AgentCommand>> {
        if let Some(resident) = self.agents.get(&agent) {
            return Some(resident.actor.clone());
        }
        let runner = state.runner_of(agent)?;
        let record = state.record(runner)?;
        let settings = record.state.settings(agent)?.clone();
        let role = AgentRole::of(record.kind, runner == state.root);
        self.spawn_agent(ctx, state, agent, role, settings)
            .map(|r| r.actor)
    }
```

- [ ] **Step 1: Write the failing test**

```rust
// crates/server/src/sessions/session_actor/testing.rs
/// The session's status is the root runner's, folded — never a second variable
/// beside it. Thirteen `report(LITERAL)` calls each restated the status the
/// next line was about to fold, and that is the drift this closes.
#[tokio::test]
async fn the_reported_status_is_the_root_runners_folded_status() {
    let h = harness().await;
    h.record_spec().await;
    assert_eq!(h.last_reported_status().await, SessionStatus::Provisioning);
    h.runtime_ready().await;
    // A background worker must not make the session read Running.
    h.spawn_worker("read the flake").await;
    assert_eq!(h.last_reported_status().await, SessionStatus::Idle);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib session_actor::testing::the_reported_status`
Expected: FAIL to compile — the harness does not exist against the new state yet. That is the failure; it becomes a real assertion in Task 7.

- [ ] **Step 3: Write the implementation**

```rust
#[async_trait]
impl EventSourcedActor for SessionActor {
    type Command = SessionInbox;
    type Event = SessionEvent;
    type State = SessionState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.id)
    }

    fn initial_state() -> SessionState {
        SessionState::default()
    }

    /// One writer, and it is the state's own. The twenty-variant match this
    /// replaces routed each event to the component that understood it; a
    /// runner event is addressed to its runner and the rest is the session's.
    fn apply_event(mut state: SessionState, event: SessionEvent) -> SessionState {
        state.apply(&event);
        state
    }

    async fn on_events_persisted(&mut self, events: &[SessionEvent], state: &SessionState) {
        self.record_lifecycle(events, state).await;
        self.report_forks(state).await;
        self.report_status(state).await;
    }

    async fn handle_command(
        &mut self,
        state: &SessionState,
        cmd: SessionInbox,
        ctx: &mut ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        self.dispatch(state, cmd.cmd, ctx).await
    }

    async fn on_recovery_complete(
        &mut self,
        state: &SessionState,
        ctx: &mut ActorContext<SessionInbox>,
    ) {
        self.services = resolve(&self.users, &self.account).await;
        let Some(spec) = state.spec.clone() else {
            return;
        };
        self.adopt(spec, state, ctx).await;
    }
}
```

`record_lifecycle` now fans out through the table Phase A already wrote:

```rust
    /// Write what just became durable into the agents' own transcripts.
    ///
    /// The table lives in `runners::lifecycle_routing`, which answers only "who
    /// needs to see this"; the session decides, journals and folds. A fact can
    /// belong in more than one log — a worker's result is both its own last word
    /// and news to the parent waiting on it — so this is a list, not a
    /// destination.
    async fn record_lifecycle(&mut self, events: &[SessionEvent], state: &SessionState) {
        // Stamped once for the whole batch, not per entry: these events became
        // durable together, and a clock read per entry would order them by when
        // this loop happened to reach each one.
        let at_ms = now_ms();
        for event in events {
            for (agent, entry) in crate::sessions::runners::lifecycle_routing::route(event, state) {
                let Some(resident) = self.agents.get(&agent) else {
                    continue;
                };
                let _ = resident
                    .actor
                    .tell(AgentCommand::RecordLifecycle {
                        event: entry,
                        at_ms,
                    })
                    .await;
            }
        }
    }
```

`report_forks` reads the projection instead of walking a roster:

```rust
    async fn report_forks(&mut self, state: &SessionState) {
        let forks = crate::sessions::runners::reads::fork_rows(state);
        if forks == self.last_reported_forks {
            return;
        }
        self.last_reported_forks = forks.clone();
        let _ = self
            .supervisor
            .tell(SessionSupervisorCommand::ForksChanged {
                id: self.id.to_string(),
                forks,
            })
            .await;
    }
```

`report_status` reads `runners::reads::session_status(state)` in place of `state.status`.

- [ ] **Step 4: Verify the fold**

Run: `cargo test -p horsie-server --lib runners::state`
Expected: PASS — Phase A's fold tests still green, now reached through the actor's `apply_event`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(sessions)!: the session folds through its runners"
```

---

### Task 4: dispatch by owner

**Files:**
- Modify: `crates/server/src/sessions/session_actor/mod.rs` (add `dispatch`)
- Modify: `crates/server/src/sessions/session_actor/types.rs` (`SessionCommand` shrinks)

**Interfaces:**
- Consumes: `reach` (Task 2), `SessionState::runner_of`, `runners::reads::resolve`.
- Produces: `async fn dispatch(&mut self, state, cmd: SessionCommand, ctx) -> CommandEffect<SessionEvent>`; `SessionCommand` with eight variants. Task 5 adds the boundary behind it; Task 8 deletes the six command groups this replaces.

**The new vocabulary.** `SessionCommand` becomes:

```rust
pub enum SessionCommand {
    /// A tool call one of this session's agents made that the session answers.
    AgentTool { agent: AgentId, call: ToolCall, reply: ReplyTo<SessionReply> },
    /// A message for one of this session's agents.
    UserMessage { agent_id: Option<String>, text: String, reply: ReplyTo<Result<MessageAccepted, UserMessageError>> },
    Stop { agent_id: String, reply: ReplyTo<Result<(), String>> },
    Answer { agent_id: String, answers: Vec<AskAnswer>, reply: ReplyTo<Result<(), AnswerError>> },
    Read(ReadCommand),
    Hooks(HookCommand),
    Core(CoreCommand),
    /// Internal: an agent reported its terminal outcome.
    AgentOutcome(AgentOutcome),
}
```

`LifecycleCommand`, `TurnCommand`, `RunCommand`, `SubAgentCommand` and `ForkCommand` all go. Provisioning is no longer a command at all — it is `Action::Provision`, asked for by a `Pending` runtime runner, which is what gives it an answer at recovery: a session whose sandbox died between the ask and the answer used to sit `Pending` with nothing to restart it.

- [ ] **Step 1: Write the failing test**

```rust
/// Every agent-addressed command resolves through one map. The probe this
/// replaces tried three registries in a fixed order, and the order was
/// load-bearing: answering `Sub` before checking forks made a fork of a fork
/// read as a fork of a subagent.
#[tokio::test]
async fn a_fork_of_a_fork_resolves_to_its_own_runner() {
    let h = harness().await;
    h.record_spec().await;
    h.runtime_ready().await;
    let first = h.fork_of(h.root_agent().await, "one").await;
    let second = h.fork_of(first, "two").await;
    assert_ne!(h.runner_of(second).await, h.runner_of(first).await);
    assert_eq!(h.parent_of(h.runner_of(second).await).await, Some(first));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib a_fork_of_a_fork`
Expected: FAIL — no `fork_of` on the harness.

- [ ] **Step 3: Write the implementation**

```rust
    /// Route one command to whatever owns what it means.
    ///
    /// Two kinds only: addressed to an agent, or the session's own. There is no
    /// third — the six command groups this replaces were six ways of saying
    /// "which of my four kinds of agent is this", and that question is now
    /// `state.agents[&id]`.
    async fn dispatch(
        &mut self,
        state: &SessionState,
        cmd: SessionCommand,
        ctx: &mut ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            SessionCommand::AgentTool { agent, call, reply } => {
                self.on_agent_tool(state, agent, call, reply, ctx).await
            }
            SessionCommand::UserMessage { agent_id, text, reply } => {
                self.on_user_message(state, agent_id.as_deref(), text, reply, ctx).await
            }
            SessionCommand::Stop { agent_id, reply } => {
                self.on_stop(state, &agent_id, reply, ctx).await
            }
            SessionCommand::Answer { agent_id, answers, reply } => {
                self.on_answer(state, &agent_id, answers, reply, ctx).await
            }
            SessionCommand::Read(c) => Reads::handle(self, state, c, ctx).await,
            SessionCommand::Hooks(c) => HookRouting::handle(self, state, c, ctx).await,
            SessionCommand::Core(c) => SessionCore::handle(self, state, c, ctx).await,
            SessionCommand::AgentOutcome(outcome) => {
                self.on_agent_outcome(state, outcome, ctx).await
            }
        }
    }

    /// An agent's own ending, handed to the runner that owns it.
    ///
    /// The one command routed by identity rather than by variant, and it stays
    /// that way — but the identity probe is gone. The same `Concluded` still
    /// means three different things; which one is now decided by the runner the
    /// agent belongs to, which is a lookup rather than an inference.
    async fn on_agent_outcome(
        &mut self,
        state: &SessionState,
        outcome: AgentOutcome,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let (agent, end) = match TurnEnd::split(outcome) {
            Ok(pair) => pair,
            // Usage is banked for every agent alike, and always: the tokens were
            // spent whatever became of the turn that spent them.
            Err(banked) => return self.bank(state, banked, ctx).await,
        };
        let Some(runner) = state.runner_of(agent) else {
            tracing::warn!(session = %self.id, %agent, "an outcome from an agent no runner owns");
            return CommandEffect::none();
        };
        let Some(record) = state.record(runner) else {
            return CommandEffect::none();
        };
        let Some(lifecycle) = record.state.lifecycle() else {
            return CommandEffect::none();
        };
        let emit = lifecycle.on_agent_ended(agent, &end);
        self.persist_and_advance(state, self.wrap(runner, emit), ctx).await
    }
```

`wrap` turns a runner's `Emit` into session events, stamping the journal time once:

```rust
    /// A runner's events, addressed and stamped.
    ///
    /// `at_ms` is read here — the one place it may be — because it is a fact
    /// about the journal entry rather than about what was decided. A decision is
    /// made once and folded any number of times, so a clock inside a fold would
    /// give a replay different timestamps from the live run.
    fn wrap(&self, runner: RunnerId, emit: Emit) -> Vec<SessionEvent> {
        let at_ms = now_ms();
        emit.events
            .into_iter()
            .map(|event| SessionEvent::Runner { id: runner, event: Box::new(event), at_ms })
            .collect()
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-server --lib session_actor`
Expected: PASS for the resolution tests. Others still red until Task 7 ports them.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(sessions)!: one lookup where three registries were probed in order"
```

---

### Task 5: the boundary

**Files:**
- Modify: `crates/server/src/sessions/session_actor/mod.rs` (`perform`, `flush_then_drain`, `persist_and_advance`)

**Interfaces:**
- Consumes: `view`/`busy`/`next_actions`/`owed` (already on the branch, commit `0ef59ebb`), `birth::born` (Task 1), `reach` (Task 2), `wrap` (Task 4).
- Produces: `async fn perform(&mut self, runner: RunnerId, action: Action, state, ctx) -> Vec<SessionEvent>`.

**The two orderings, and getting either backwards is the easiest mistake here.**

- **Creation persists first.** `RunnerCreated` is durable before the child's agent exists, and the tool's reply fires only after the journal ack. A crash between spawn and persist would hand the model an id for an agent that does not exist; a crash before the ack replays as no child at all, which is strictly better.
- **Delivery tells first.** The report is enqueued into the parent's agent, and only then is the acknowledgement persisted. A crash in that window replays as a report still owed, and it is delivered again — at-least-once, never lost.

Which makes delivery two batches, not one. One batch would leave no re-drive point.

- [ ] **Step 1: Write the failing tests**

```rust
/// Delivery is at-least-once, and the capability's own `outstanding` is the
/// single fact recording both whether the parent has been told and who to tell.
/// A crash between the tell and the persist replays as a report still owed.
#[tokio::test]
async fn a_report_not_yet_acknowledged_is_delivered_again() {
    let h = harness().await;
    h.record_spec().await;
    h.runtime_ready().await;
    let parent = h.root_agent().await;
    let child = h.spawn_worker_from(parent, "read the flake").await;
    h.conclude(child, json!({"found": "a race"})).await;
    h.cut_journal_before_ack().await;
    let reloaded = h.reload().await;
    assert_eq!(reloaded.enqueued_reports(parent).await.len(), 1);
}

/// Creation persists first: a crash before the ack replays as no child at all,
/// never as an agent nothing tracks.
#[tokio::test]
async fn a_create_cut_before_its_ack_replays_as_no_child() {
    let h = harness().await;
    h.record_spec().await;
    h.runtime_ready().await;
    h.spawn_worker("read the flake").await;
    h.cut_journal_before_runner_created().await;
    let reloaded = h.reload().await;
    assert!(reloaded.runners_of_kind(RunnerKind::SubAgent).await.is_empty());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server --lib a_report_not_yet_acknowledged`
Expected: FAIL — no `cut_journal_before_ack` on the harness.

- [ ] **Step 3: Write the implementation**

```rust
    /// Carry out one runner's decision and return the events that record it.
    async fn perform(
        &mut self,
        runner: RunnerId,
        action: Action,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        match action {
            Action::StartAgent { agent, equipment, settings, first } => {
                self.start_agent(runner, agent, equipment, *settings, first, state, ctx).await
            }
            Action::CreateChild { id, kind, args, parent } => {
                self.create_child(id, kind, args, parent, state, ctx).await
            }
            Action::Deliver { to, from, part } => self.deliver(to, from, *part, state, ctx).await,
            Action::Cancel { agent } => {
                self.cancel_agent(agent, state, ctx).await;
                Vec::new()
            }
            Action::Provision => self.provision(state, ctx).await,
            Action::Reply { text } => {
                // Answered on the call that is waiting, never journaled: a
                // refusal is not something that happened to this session.
                self.answer_pending(runner, text).await;
                Vec::new()
            }
        }
    }

    /// Put a finished child's report in the queue of the agent owed it.
    ///
    /// Tell-then-persist. Skipped rather than failed when the agent cannot be
    /// reached: the report stays owed and the next boundary tries again.
    async fn deliver(
        &mut self,
        to: AgentId,
        from: RunnerId,
        part: SubAgentResultPart,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        let Some(agent) = self.reach(to, state, ctx) else {
            return Vec::new();
        };
        if agent
            .tell(AgentCommand::Enqueue {
                item: Incoming::SubAgent { id: from.to_string(), part: Box::new(part) },
                ack: None,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        // The acknowledgement is the *capability's*, not the session's: the
        // parent's `SubAgentCapability` folds `Reported` and drops the child
        // from `outstanding`. So this returns nothing and the capability's own
        // decision carries the write.
        Vec::new()
    }

    /// Everything startable at this boundary, performed in order, each seeing
    /// the state the previous one produced.
    ///
    /// Deliveries first: a parent waiting on its children is work already in
    /// flight, and a next turn can wait a boundary.
    async fn flush_then_drain(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let mut next = state.clone();
        for (child, parent, outcome) in self.owed(&next) {
            let produced = self.offer_to_parent(child, parent, outcome, &next, ctx).await;
            for e in &produced {
                next.apply(e);
            }
            events.extend(produced);
        }
        for (runner, action) in self.next_actions(&next) {
            let produced = self.perform(runner, action, &next, ctx).await;
            for e in &produced {
                next.apply(e);
            }
            events.extend(produced);
        }
        events
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-server --lib session_actor::testing`
Expected: the two recovery tests PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(sessions): the boundary performs what runners ask, and re-drives"
```

---

### Task 6: reads and hooks rebound

Finding 2: this is rebinding, not rewriting. `runners/reads.rs` and `runners/lifecycle_routing.rs` already hold the logic, tested.

**Files:**
- Modify: `crates/server/src/sessions/session_actor/reads.rs` (759 lines → a delegation)
- Modify: `crates/server/src/sessions/session_actor/hooks.rs` (routes by `AgentId`)
- Modify: `crates/server/src/http/handlers.rs` (`to_wire_agent`, `MAIN_AGENT_ID`)

**Interfaces:**
- Consumes: every `pub fn` in `runners::reads` — `session_status`, `agent_roster`, `agent_entry`, `settings_of`, `task_and_output`, `run_state`, `run_graph`, `usage_stats`, `fork_rows`, `resolve`.
- Produces: `ReadCommand` handling unchanged from the caller's side. `supervisor.rs` reply types keep their shapes.

- [ ] **Step 1: Write the failing test**

```rust
/// `"main"` is a name, not an id. It resolves to the root runner's agent — or,
/// when the root is a run, to the step in flight — and is no longer the
/// session's own uuid.
#[tokio::test]
async fn main_resolves_to_the_root_runners_agent() {
    let h = harness().await;
    h.record_spec().await;
    h.runtime_ready().await;
    let root = h.root_agent().await;
    assert_eq!(h.read_agent(Some("main")).await.map(|e| e.id), Some(root.to_string()));
    assert_eq!(h.read_agent(None).await.map(|e| e.id), Some(root.to_string()));
    assert_ne!(root.to_string(), h.session_id().to_string());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib main_resolves_to_the_root`
Expected: FAIL — resolves to the session uuid.

- [ ] **Step 3: Write the implementation**

`session_actor/reads.rs` becomes delegation, one arm per `ReadCommand`:

```rust
            ReadCommand::Agent { agent_id, reply } => {
                let entry = reads::resolve(state, agent_id.as_deref())
                    .and_then(|agent| reads::agent_entry(state, agent));
                let _ = reply.send(entry.map(AgentDetail::from));
                CommandEffect::none()
            }
            ReadCommand::Snapshot { reply } => {
                let _ = reply.send(SessionSnapshot {
                    status: reads::session_status(state),
                    usage: reads::usage_stats(state),
                    agents: reads::agent_roster(state),
                });
                CommandEffect::none()
            }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-server --lib reads`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(sessions): the read side is the runner projection"
```

---

### Task 7: port the session actor tests

A green port is the acceptance criterion for the whole swap — it is what says the rewrite preserved behaviour rather than merely compiling.

**Files:**
- Modify: `crates/server/src/sessions/session_actor/testing.rs` (1,679 lines)
- Modify: `crates/server/src/sessions/events.rs` (`fold_session_state` rebinds to `SessionState::apply`)
- Modify: `crates/tests/tests/agent_recovery_e2e.rs`

- [ ] **Step 1: Rebind the fold helpers**

```rust
// crates/server/src/sessions/events.rs
#[cfg(test)]
pub(crate) fn fold_session_state(events: &[SessionEvent]) -> SessionState {
    let mut state = SessionState::default();
    for event in events {
        state.apply(event);
    }
    state
}
```

- [ ] **Step 2: Run the suite and triage**

Run: `cargo test -p horsie-server --lib sessions::`
Expected: a list of failures. Triage each into exactly one of: *ported* (same behaviour, new vocabulary), *deleted with its component* (it tested a probe that no longer exists), or **a real regression**. Write the third list down before fixing any of it.

- [ ] **Step 3: Run the e2e suites**

Run: `TMPDIR=/tmp cargo test -p horsie-tests --test session_e2e`
Then: `TMPDIR=/tmp cargo test -p horsie-tests --test agent_recovery_e2e`
Expected: 36 session e2e and the recovery suite green. `-p horsie-server --lib` is a false green for these paths.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "test(sessions): the actor's behaviour, over runners"
```

---

### Task 8: delete the old vocabulary

**Test:** `cargo clippy --locked --all-targets --all-features -- -D warnings` is the test. A dead type is a warning, and the deletion is complete when nothing names the old vocabulary.

- [ ] **Step 1: Delete the files**

```bash
git rm crates/server/src/sessions/session_actor/fork.rs \
       crates/server/src/sessions/session_actor/subagent.rs \
       crates/server/src/sessions/session_actor/turns.rs \
       crates/server/src/sessions/session_actor/run.rs \
       crates/server/src/sessions/session_actor/lifecycle.rs \
       crates/server/src/sessions/session_actor/component.rs \
       crates/server/src/sessions/forks.rs \
       crates/server/src/sessions/subagents.rs \
       crates/server/src/sessions/lifecycle_routing.rs \
       crates/server/src/sessions/orchestrator.rs
```

Remove their `mod` declarations from `sessions/mod.rs` and `session_actor/mod.rs`.

- [ ] **Step 2: Delete the types the files left behind**

From `session_actor/types.rs`: the old `SessionState`, `SessionDomainEvent`, `LifecycleCommand`, `TurnCommand`, `RunCommand`, `SubAgentCommand`, `ForkCommand`. From wherever they survive: `effective_settings`, `effective_settings_for_parent`.

**Keep** the `AskAnswer`/`AnswerError` re-export.

- [ ] **Step 3: Run clippy and fix what it names**

Run: `cargo clippy --locked --all-targets --all-features -- -D warnings`
Expected: clean. Every error is either a call site to rebind or a type to delete.

- [ ] **Step 4: Confirm nothing names the old vocabulary**

```bash
git grep -n "AgentKey\|SessionAgentKind\|SubAgentForest\|TreeOwner\|root_owner\|owner_for\|SessionDomainEvent" -- crates/ ':!*/plans/*' ':!*/specs/*'
```

Expected: no output.

- [ ] **Step 5: Full verification, then commit**

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --workspace
git add -A
git commit -m "refactor(sessions)!: delete the four-kind vocabulary the runners replaced"
```

---

## Out of scope, deliberately

- **B6 — invariant 6** (an agent may not conclude with outstanding children). Its test must be written first and watched to fail against Task 5's code: it is the invariant that licenses one `SubAgentCapability` for all three parent kinds, so a test that passes immediately has proved nothing.
- **B7 — the cancel cascade and the session-wide cap.**
- **Phase C — `invoke_workflow`.**

These are the remaining Phase B/C tasks. They are follow-ups because the swap is already an atomic state-shape change: there is no intermediate commit in which half the session reads `state.run` and half reads `state.runners`, and adding behaviour to that same change would make it unreviewable.

## Self-review notes

- **Spec coverage.** Locked decisions 1–9 all land in Tasks 1–8 except 8 (durable workspace scan, already shipped in `RuntimeCapability`) and 9 (per-agent plugins, already true). Invariants 1–5 and 7–11 are Phase A properties this plan preserves; invariant 6 is explicitly deferred above.
- **Type consistency.** `born(args, capabilities, run)` in Task 1 is called with the same three arguments in Task 5's `create_child`. `reach(agent, state, ctx)` in Task 2 is called with that signature in Tasks 5 and 6. `wrap(runner, emit)` in Task 4 is used in Tasks 4 and 5.
- **Known soft spot.** Task 7 Step 2 cannot enumerate its failures in advance — the triage *is* the work, and the instruction that matters is to write the regression list down before fixing any of it.
