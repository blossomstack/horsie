# Session Runners and Capabilities Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the session actor's four hard-coded agent kinds with runners that own units of work and capabilities that equip their agents, so any agent can create any number of subagents and workflow runs, nested arbitrarily.

**Architecture:** A session hosts runners keyed by `RunnerId`; each runner owns one agent role and holds a list of capabilities. Capability state nests inside runner state, which nests inside `SessionState`, so there is one journal and recovery is the existing fold. Runners and capabilities decide and return `(events, actions)`; the session actor performs. Phase A builds this as pure logic with no actor, no runtime and no journal; Phase B wires it into `SessionActor` and deletes the old components; Phase C ships the `invoke_workflow` tool.

**Tech Stack:** Rust 2024, `horsie-actor` event-sourced actors, `serde` for the journal, `tokio`, `cargo test`.

**Spec:** `docs/superpowers/specs/2026-08-15-session-runners-design.md`

## Global Constraints

- Repo: `horsie`, worktree `.claude/worktrees/session-runners`, branch `design/session-runners`.
- CI runs `cargo fmt --all -- --check` and `cargo clippy --locked --all-targets --all-features -- -D warnings`. Clippy without `-D warnings` exits 0 on code CI rejects — always pass the flag locally.
- Iterate with `cargo test -p horsie-server --lib <filter>`. Do not run the full workspace suite; CI is the backstop.
- The journal shape breaks deliberately. Do not add compatibility shims, `#[serde(default)]` bridges to old field names, or migration code.
- Every capability is named `XxxxCapability` and lives in its own file under `crates/server/src/sessions/runners/capabilities/`.
- Inner types are module-scoped and plain: `sub_agent::Event`, `sub_agent::Request` — never `SubAgentCapabilityEvent`.
- A runner impl holds no fields. All state arrives as a `&State` argument.
- Decide, never perform: every handler returns a `Decision { events, actions }`. No I/O, no clock, no `Uuid::new_v4()` inside a fold. The single exception is `Capability::setup`, which is async and runs on the agent's own task.
- Commit messages: short subject, no body unless the diff hides context. No AI attribution trailers.

---

## Phase A — the core, as pure logic (PR 1)

Phase A adds `crates/server/src/sessions/runners/` and nothing else. It is not wired into `SessionActor`, so the module carries `#![allow(dead_code)]` at its root until Phase B removes it. Every task is unit-testable against hand-built state with no actor, no runtime and no journal — which is the property the whole design rests on, so proving it here is the point rather than a side effect.

### File structure

```
crates/server/src/sessions/runners/
  mod.rs              Runner trait, AgentLifecycle, RunnerState, RunnerEvent, assembly
  ids.rs              RunnerId, AgentId, RunnerKind, RunnerStatus
  state.rs            SessionState, RunnerRecord, and the session-level fold
  message.rs          Message, ChildMsg, ChildOutcome, AskMsg, Routing
  action.rs           Action, AgentSpec, ToolLayer, PromptSection
  conversation.rs     ConversationRunner + conversation::State/Event
  subagent.rs         SubAgentRunner + subagent::State/Event
  workflow.rs         WorkflowRunner + workflow::State/Event
  runtime.rs          RuntimeRunner + runtime::State/Event
  capabilities/
    mod.rs            Capability trait, CapEvent, Decision, the offer
    runtime.rs        RuntimeCapability
    memory.rs         MemoryCapability
    mcp.rs            McpCapability
    control_plane.rs  ControlPlaneCapability
    ask_user.rs       AskUserCapability
    title.rs          TitleCapability
    sub_agent.rs      SubAgentCapability
    workflow.rs       WorkflowCapability
    fork.rs           ForkCapability
    step_result.rs    StepResultCapability
```

**Superseded — see the spec.** Phase A built `AgentSpec` as a *description* of toolbox layers, so that `setup` could stay synchronous and run at decision time. Design review replaced that: `setup` is async, runs on the agent's own task, and fills in the real `AgentSpec` the loop runs with. What Phase A shipped still compiles and passes; reconciling it is the first task of Phase B, and it removes code rather than adding it.

### Names as built, and what review changed

A1 and A2 settled a vocabulary; the design review that followed replaced part of it. Both are recorded, because the code on disk is still the first column:

| Phase A shipped | The spec now says | Why |
|---|---|---|
| `capabilities::Handler` trait | `Capability` trait | `Handler` named nothing; the enum that was holding the name is gone |
| `Capability` closed enum, `dispatch!` macro | `Vec<Box<dyn Capability>>` | the list is composed at runtime; a runner that should not delegate is not given the capability |
| `Decision = (Vec<CapEvent>, Vec<Action>)` | `struct Decision { events, actions }` | a tuple at the centre of every handler |
| `fn setup(&self, spec: &mut AgentSpec)` | `async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError>` plus `async fn teardown` | setup acquires runtimes and connects MCP; it was never synchronous work |
| `ToolLayer`, a described layer | the real toolbox, assembled in `setup` | the description needed a nameless third party to realise it |

Unchanged and still correct:

- `Caller { agent, depth, active_agents }` — what a capability learns about the world outside its slice, and where the recursion budget and the session-wide concurrency cap are read.
- `offer(caps, caller, &Message) -> Option<Decision>` — the tool-call scan; structural messages are addressed to their owner instead.
- `Action::Reply { text }` — a call that answers with a message rather than an effect.
- `AgentSettings` has a `plugins: Vec<String>` field, easy to miss when hand-building one in a test.

One correction for A8, found while reading the existing code: `WorkflowRunSpec::step_agent_id(session_id, index)` derives a step's agent from the **session** id, which was safe when a session had at most one run. With concurrent runs it collides. Key it on the **runner** id instead.

---

### Task A1: ids and session state

**Files:**
- Create: `crates/server/src/sessions/runners/ids.rs`
- Create: `crates/server/src/sessions/runners/state.rs`
- Create: `crates/server/src/sessions/runners/mod.rs` (module declarations only, for now)
- Modify: `crates/server/src/sessions/mod.rs` — add `pub mod runners;`

**Interfaces:**
- Produces: `RunnerId(Uuid)`, `AgentId(Uuid)` (both `Copy`, `Ord`, `Serialize`, `Deserialize`, `Display`); `RunnerKind::{Conversation, SubAgent, Workflow, Runtime}`; `RunnerStatus::{Pending, Running, AwaitingInput, Done, Failed, Cancelled}`; `RunnerRecord`; `SessionState` with `root`, `agents`, `usage`, `runners`; `SessionState::status() -> RunnerStatus`; `SessionState::runner_of(AgentId) -> Option<RunnerId>`; `SessionState::children_of(AgentId) -> Vec<RunnerId>`; `SessionState::depth_of(RunnerId) -> u32`.

- [ ] **Step 1: Write the failing tests**

Create `crates/server/src/sessions/runners/state.rs` with the test module first:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The session's status is the root runner's, not an aggregate: a
    /// background subagent must not make the session read Running.
    #[test]
    fn session_status_is_the_root_runners() {
        let mut s = SessionState::default();
        let root = s.insert_for_test(RunnerKind::Conversation, None, RunnerStatus::Pending);
        s.root = root;
        let agent = AgentId::new_v4();
        s.agents.insert(agent, root);
        let _busy = s.insert_for_test(RunnerKind::SubAgent, Some(agent), RunnerStatus::Running);
        assert_eq!(s.status(), RunnerStatus::Pending);
    }

    /// Nesting is recorded once, in `parent`. Depth is a walk up it, which is
    /// what replaces MAX_SUBAGENT_DEPTH's per-tree bookkeeping.
    #[test]
    fn depth_walks_up_the_parent_chain() {
        let mut s = SessionState::default();
        let root = s.insert_for_test(RunnerKind::Conversation, None, RunnerStatus::Running);
        s.root = root;
        let a0 = AgentId::new_v4();
        s.agents.insert(a0, root);
        let r1 = s.insert_for_test(RunnerKind::SubAgent, Some(a0), RunnerStatus::Running);
        let a1 = AgentId::new_v4();
        s.agents.insert(a1, r1);
        let r2 = s.insert_for_test(RunnerKind::SubAgent, Some(a1), RunnerStatus::Running);

        assert_eq!(s.depth_of(root), 0);
        assert_eq!(s.depth_of(r1), 1);
        assert_eq!(s.depth_of(r2), 2);
    }

    #[test]
    fn children_of_an_agent_are_the_runners_it_parented() {
        let mut s = SessionState::default();
        let root = s.insert_for_test(RunnerKind::Conversation, None, RunnerStatus::Running);
        s.root = root;
        let a0 = AgentId::new_v4();
        s.agents.insert(a0, root);
        let r1 = s.insert_for_test(RunnerKind::SubAgent, Some(a0), RunnerStatus::Running);
        let r2 = s.insert_for_test(RunnerKind::Workflow, Some(a0), RunnerStatus::Running);
        let other = AgentId::new_v4();
        s.agents.insert(other, r1);
        let _elsewhere = s.insert_for_test(RunnerKind::SubAgent, Some(other), RunnerStatus::Running);

        let mut kids = s.children_of(a0);
        kids.sort();
        let mut want = vec![r1, r2];
        want.sort();
        assert_eq!(kids, want);
    }

    /// Usage is aggregated by model, not by agent: per-agent totals belong to
    /// the runner that owns the agent.
    #[test]
    fn usage_aggregates_by_model() {
        /// `UsageTotal` has public counters and a `combine`; there is no
        /// constructor helper, so build one here rather than adding a
        /// production API only tests want.
        fn spent(input: u64, output: u64) -> UsageTotal {
            UsageTotal {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            }
        }
        let mut s = SessionState::default();
        s.bank("sonnet".into(), spent(10, 5));
        s.bank("sonnet".into(), spent(1, 1));
        s.bank("opus".into(), spent(2, 2));
        assert_eq!(s.usage.len(), 2);
        assert_eq!(s.usage["sonnet"].input_tokens, 11);
    }

    /// The state is snapshotted, so a row written before a field existed must
    /// still load. Container-level default, additive fields only.
    #[test]
    fn an_empty_row_deserializes() {
        let s: SessionState = serde_json::from_str("{}").unwrap();
        assert!(s.runners.is_empty());
        assert!(s.agents.is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server --lib sessions::runners::state`
Expected: FAIL — `cannot find type SessionState in this scope` / unresolved module.

- [ ] **Step 3: Write the implementation**

`crates/server/src/sessions/runners/ids.rs`:

```rust
//! The two id spaces a session addresses, and the vocabulary every runner
//! record is described in.
//!
//! `RunnerId` and `AgentId` are distinct newtypes over `Uuid` on purpose: the
//! old code had one flat uuid space in which a fork, a subagent and a step were
//! told apart by probing three registries in a fixed order, and getting that
//! order wrong made a fork of a fork read as a fork of a subagent.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new_v4() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(RunnerId, "One unit of work a session hosts.");
id_type!(AgentId, "One agent a runner started. Its journal is `agent/<id>`.");

/// What a runner is. Decides which impl the session instantiates for a record,
/// and nothing else — every behavioural difference lives in that impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerKind {
    Conversation,
    SubAgent,
    Workflow,
    Runtime,
}

/// The same six words for every kind of runner. What each one *means* is the
/// runner's business; that they are spelled once is what stops a session's
/// status and a runner's disagreeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RunnerStatus {
    #[default]
    Pending,
    Running,
    AwaitingInput,
    Done,
    Failed,
    Cancelled,
}

impl RunnerStatus {
    /// Whether this runner will start nothing further by itself.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}
```

`crates/server/src/sessions/runners/state.rs`:

```rust
//! What a session knows: which runners exist, how they nest, and the totals.
//!
//! Nothing here belongs to one agent. A runner's own slice lives in
//! [`RunnerRecord::state`], and the session never looks inside it — it hands it
//! back to the runner that owns it. Per-agent usage lives there too; what the
//! session keeps is an aggregate by model.

use super::ids::{AgentId, RunnerId, RunnerKind, RunnerStatus};
use super::RunnerState;
use crate::agent_loop::UsageTotal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One runner, as the session records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerRecord {
    pub kind: RunnerKind,
    /// The agent that created me. `None` for the root and for the runtime.
    ///
    /// Provenance, not debt: whether a runner reports is decided by its kind,
    /// which is why a fork can have a parent and owe it nothing.
    pub parent: Option<AgentId>,
    pub status: RunnerStatus,
    /// My slice, opaque to the session.
    pub state: RunnerState,
    pub created_at_ms: u64,
    pub ended_at_ms: u64,
}

/// Purely structure and aggregates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub spec: Option<crate::sessions::spec::SessionSpec>,
    /// The conversation or the run this session *is*.
    pub root: RunnerId,
    /// Which runner owns each agent. Structure, not content — what an agent
    /// said lives in the agent's own journal.
    pub agents: BTreeMap<AgentId, RunnerId>,
    /// Tokens by model across everything this session has run.
    pub usage: BTreeMap<String, UsageTotal>,
    pub runners: BTreeMap<RunnerId, RunnerRecord>,
}

impl Default for RunnerId {
    fn default() -> Self {
        Self(uuid::Uuid::nil())
    }
}

impl SessionState {
    /// The session's status: the root runner's, never an aggregate. A busy
    /// subagent is not the session working.
    #[must_use]
    pub fn status(&self) -> RunnerStatus {
        self.runners
            .get(&self.root)
            .map_or(RunnerStatus::Pending, |r| r.status)
    }

    #[must_use]
    pub fn runner_of(&self, agent: AgentId) -> Option<RunnerId> {
        self.agents.get(&agent).copied()
    }

    /// The runners this agent created. The only direction `parent` is read in
    /// bulk; a single child's owner is a field lookup.
    #[must_use]
    pub fn children_of(&self, agent: AgentId) -> Vec<RunnerId> {
        self.runners
            .iter()
            .filter(|(_, r)| r.parent == Some(agent))
            .map(|(id, _)| *id)
            .collect()
    }

    /// How deep this runner sits. The walk that replaces the per-tree depth
    /// bookkeeping, and the one budget a nested workflow needs.
    #[must_use]
    pub fn depth_of(&self, runner: RunnerId) -> u32 {
        let mut depth = 0;
        let mut at = runner;
        // Bounded by the number of runners: `parent` is written once at
        // creation and never edited, so the chain cannot contain a cycle.
        while let Some(parent) = self.runners.get(&at).and_then(|r| r.parent) {
            let Some(next) = self.runner_of(parent) else {
                break;
            };
            depth += 1;
            at = next;
        }
        depth
    }

    /// Bank tokens against the model that spent them.
    pub fn bank(&mut self, model: String, spent: UsageTotal) {
        let entry = self.usage.entry(model).or_default();
        *entry = entry.combine(&spent);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
impl SessionState {
    /// Insert a bare record. Tests only — live code creates a runner by
    /// journaling `RunnerCreated`, never by reaching in here.
    pub(crate) fn insert_for_test(
        &mut self,
        kind: RunnerKind,
        parent: Option<AgentId>,
        status: RunnerStatus,
    ) -> RunnerId {
        let id = RunnerId::new_v4();
        self.runners.insert(
            id,
            RunnerRecord {
                kind,
                parent,
                status,
                state: RunnerState::for_kind(kind),
                created_at_ms: 0,
                ended_at_ms: 0,
            },
        );
        id
    }
}
```

`crates/server/src/sessions/runners/mod.rs`:

```rust
//! A session hosts runners; a runner owns one agent role and the capabilities
//! its agents are equipped with.
//!
//! Nothing in this module performs anything. A runner and a capability both
//! answer with `(events, actions)`: the events are what to journal, the actions
//! are what the session should do. That is what makes every decision here
//! testable against a hand-built state with no actor, no runtime and no
//! journal — and it is why `actions()` replaced the `run()` an earlier draft
//! had, since a pure answer is identical whether the state arrived a
//! millisecond ago or from a journal replayed after a restart.
#![allow(dead_code)] // Phase A: built and tested here, wired in Phase B.

pub mod capabilities;
pub mod ids;
pub mod state;

pub use ids::{AgentId, RunnerId, RunnerKind, RunnerStatus};
pub use state::{RunnerRecord, SessionState};

use serde::{Deserialize, Serialize};

/// A runner's own slice. One arm per kind; the session never matches on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerState {
    Conversation,
    SubAgent,
    Workflow,
    Runtime,
}

impl RunnerState {
    #[must_use]
    pub fn for_kind(kind: RunnerKind) -> Self {
        match kind {
            RunnerKind::Conversation => Self::Conversation,
            RunnerKind::SubAgent => Self::SubAgent,
            RunnerKind::Workflow => Self::Workflow,
            RunnerKind::Runtime => Self::Runtime,
        }
    }
}
```

The `RunnerState` arms are placeholders *in this task only* — Task A7 and A8 replace each with the real payload. That is deliberate sequencing, not an unfinished design: the state tree has to compile before the runners that fill it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-server --lib sessions::runners::state`
Expected: PASS, 5 tests.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt --all
cargo clippy -p horsie-server --all-targets --all-features -- -D warnings
git add crates/server/src/sessions/runners crates/server/src/sessions/mod.rs
git commit -m "feat(sessions): the runner state tree"
```

---

### Task A2: message, action and the capability trait

**Files:**
- Create: `crates/server/src/sessions/runners/message.rs`
- Create: `crates/server/src/sessions/runners/action.rs`
- Create: `crates/server/src/sessions/runners/capabilities/mod.rs`
- Modify: `crates/server/src/sessions/runners/mod.rs` — declare the new modules

**Interfaces:**
- Consumes: `RunnerId`, `AgentId`, `RunnerKind` (A1).
- Produces: `Message::{Tool, Command, Child, Ask}`; `ToolCall { id, name, input }`; `ChildMsg::{Outcome, Ready, Failed}`; `ChildOutcome::{SubAgent, Workflow}`; `SubAgentOutcome::{Completed, Failed}`; `WorkflowOutcome::{Finished, Failed}`; `AskMsg::Answered`; `Routing::{Offer, Owner, PendingAsk}`; `Message::routing()`; `Action::{StartAgent, CreateChild, Deliver, Cancel}`; `AgentSpec` with `layers: Vec<ToolLayer>` and `prompt: Vec<PromptSection>`; `Capability` trait with `setup`/`handle`/`apply`; `Capability` enum; `CapEvent`; `offer(&[Capability], AgentId, &Message) -> Option<(Vec<CapEvent>, Vec<Action>)>`.

- [ ] **Step 1: Write the failing tests**

In `crates/server/src/sessions/runners/message.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The outer arm carries the dispatch rule, so the session needs no table
    /// and no comment to know whether to offer a message around or address it.
    #[test]
    fn the_variant_decides_the_routing() {
        let tool = Message::Tool(ToolCall {
            id: "t1".into(),
            name: "spawn_agent".into(),
            input: serde_json::json!({}),
        });
        assert!(matches!(tool.routing(), Routing::Offer));

        let child = RunnerId::new_v4();
        let outcome = Message::Child(ChildMsg::Ready { child });
        assert!(matches!(outcome.routing(), Routing::Owner(id) if id == child));

        let ask = Message::Ask(AskMsg::Answered { answers: vec![] });
        assert!(matches!(ask.routing(), Routing::PendingAsk));
    }

    /// A fork owes nobody a result, so there is no arm in which one reports.
    /// The asymmetry is a variant that is not there rather than a check.
    #[test]
    fn a_child_outcome_has_no_fork_arm() {
        // Compile-time by construction; this asserts the two arms that exist
        // so adding a third makes someone read this test.
        let out = ChildOutcome::SubAgent(SubAgentOutcome::Completed {
            label: "audit".into(),
            report: "done".into(),
        });
        assert!(matches!(out, ChildOutcome::SubAgent(_)));
    }
}
```

In `crates/server/src/sessions/runners/capabilities/mod.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::runners::message::{Message, ToolCall};

    fn call(name: &str) -> Message {
        Message::Tool(ToolCall {
            id: "t".into(),
            name: name.into(),
            input: serde_json::json!({}),
        })
    }

    /// A fixed-name capability wins over the open-namespace one that sorts
    /// after it. Order is the conflict resolution for tool calls, so it is a
    /// property of assembly and gets a test rather than a comment.
    #[test]
    fn a_fixed_name_capability_beats_the_fallback_behind_it() {
        let caps = vec![
            Capability::Title(super::title::TitleCapability::default()),
            Capability::Runtime(super::runtime::RuntimeCapability::accepting_everything()),
        ];
        let from = AgentId::new_v4();
        let (events, _) = offer(&caps, from, &call("set_session_title")).expect("someone takes it");
        assert!(matches!(events.first(), Some(CapEvent::Title(_))));
    }

    /// A call nobody claims is an error at the one place the scan lives, never
    /// a silent drop. This replaces an exhaustive-match compile error, so it
    /// has to be loud.
    #[test]
    fn a_call_nobody_claims_is_none() {
        let caps = vec![Capability::Title(super::title::TitleCapability::default())];
        assert!(offer(&caps, AgentId::new_v4(), &call("nope")).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server --lib sessions::runners::message sessions::runners::capabilities`
Expected: FAIL — unresolved imports.

- [ ] **Step 3: Write the implementation**

`message.rs` — the enum, exactly as the spec's "Message" section defines it, plus:

```rust
/// How a message finds its capability. The variant decides, so the discipline
/// is in the type rather than in a comment: a tool call is offered around
/// because the runtime's namespace cannot be enumerated, and a child's
/// outcome is addressed because exactly one capability created that child.
pub enum Routing {
    Offer,
    Owner(RunnerId),
    PendingAsk,
}

impl Message {
    #[must_use]
    pub fn routing(&self) -> Routing {
        match self {
            Self::Tool(_) | Self::Command(_) => Routing::Offer,
            Self::Child(m) => Routing::Owner(m.child()),
            Self::Ask(_) => Routing::PendingAsk,
        }
    }
}

impl ChildMsg {
    #[must_use]
    pub fn child(&self) -> RunnerId {
        match self {
            Self::Outcome { child, .. } | Self::Ready { child } | Self::Failed { child, .. } => {
                *child
            }
        }
    }
}
```

`action.rs`:

```rust
/// Something the session should do. Every field is what the session needs to
/// perform it, so it never re-derives a decision a runner already made.
#[derive(Debug, Clone)]
pub enum Action {
    StartAgent { agent: AgentId, spec: AgentSpec, first: FirstInput },
    CreateChild { id: RunnerId, kind: RunnerKind, args: RunnerArgs, parent: AgentId },
    Deliver { to: AgentId, from: RunnerId, part: SubAgentResultPart },
    Cancel { agent: AgentId },
}

/// What an agent is equipped with, accumulated by folding its capabilities.
///
/// Layers are *described* rather than built: a `ToolLayer` names which toolbox
/// to wrap, and the context provider turns the list into real toolboxes when
/// the turn is assembled. Without that seam a `setup` test would need a
/// sandbox, and the whole point of this module is that it needs nothing.
#[derive(Debug, Clone, Default)]
pub struct AgentSpec {
    pub settings: Option<AgentSettings>,
    pub layers: Vec<ToolLayer>,
    pub prompt: Vec<PromptSection>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolLayer {
    Runtime,
    Mcp { servers: Vec<String> },
    Memory { spaces: Vec<String> },
    ControlPlane,
    AskUser,
    SessionTitle,
    ForkTitle { fork: RunnerId },
    SpawnAgent { max: u32 },
    InvokeWorkflow,
    SubmitResult { outcomes: Vec<StepOutcome>, fields: Vec<StepField> },
}
```

`capabilities/mod.rs` — the trait, the enum and the two dispatch helpers:

```rust
pub trait Capability {
    /// Equip the agent: toolbox layer, prompt section.
    fn setup(&self, spec: &mut AgentSpec);

    /// `None` means "not mine" — the runner offers it to the next capability.
    ///
    /// One method rather than a `supports` predicate beside a handler: a
    /// capability that answered yes and then could not cope, and a pair edited
    /// out of step, are states that cannot be written this way.
    fn handle(&self, from: AgentId, msg: &Message) -> Option<(Vec<CapEvent>, Vec<Action>)>;

    /// Fold my own slice.
    fn apply(&mut self, e: &CapEvent);
}

/// Offer a message to each capability until one takes it.
///
/// `&Message` rather than by value because the same message is offered
/// repeatedly; the taker clones what it keeps.
#[must_use]
pub fn offer(
    caps: &[Capability],
    from: AgentId,
    msg: &Message,
) -> Option<(Vec<CapEvent>, Vec<Action>)> {
    caps.iter().find_map(|c| c.handle(from, msg))
}
```

`Capability` is a closed enum with one arm per implementation so the list serializes into the runner's slice; implement the trait for the enum by delegation.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-server --lib sessions::runners::message sessions::runners::capabilities`
Expected: PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt --all
cargo clippy -p horsie-server --all-targets --all-features -- -D warnings
git add crates/server/src/sessions/runners
git commit -m "feat(sessions): messages, actions and the capability trait"
```

---

### Task A3: SubAgentCapability

**Files:**
- Create: `crates/server/src/sessions/runners/capabilities/sub_agent.rs`
- Modify: `crates/server/src/sessions/runners/capabilities/mod.rs` — add the `SubAgent` arm

**Interfaces:**
- Consumes: `Capability`, `Message`, `CapEvent`, `Action`, `AgentSpec`, `ToolLayer` (A2).
- Produces: `SubAgentCapability { child_settings, outstanding }`; `sub_agent::Event::{Started, Reported}`; `sub_agent::Request { label, task, agent_type }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// `spec.rs`'s own `agent_settings()` helper is `pub(super)` inside its
    /// test module and cannot be reached from here. Build one locally rather
    /// than widening a production module's test surface.
    fn settings() -> AgentSettings {
        AgentSettings {
            model: "m".into(),
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: vec![],
            memory_spaces: vec![],
            thinking_effort: None,
            max_concurrent_subagents: None,
            instructions: None,
            auto_compact: None,
            control_plane: None,
        }
    }

    fn cap() -> SubAgentCapability {
        SubAgentCapability::new(settings())
    }

    #[test]
    fn spawn_records_the_child_and_asks_for_it_to_be_created() {
        let from = AgentId::new_v4();
        let msg = Message::Tool(ToolCall {
            id: "t1".into(),
            name: "spawn_agent".into(),
            input: serde_json::json!({"label": "audit", "task": "dig"}),
        });
        let (events, actions) = cap().handle(from, &msg).expect("spawn_agent is mine");
        let CapEvent::SubAgent(Event::Started { child, from: asked_by }) = &events[0] else {
            panic!("expected Started, got {:?}", events[0]);
        };
        assert_eq!(*asked_by, from);
        let Action::CreateChild { id, kind, parent, .. } = &actions[0] else {
            panic!("expected CreateChild");
        };
        assert_eq!(id, child, "the event and the action name the same child");
        assert_eq!(*kind, RunnerKind::SubAgent);
        assert_eq!(*parent, from);
    }

    /// A child this capability did not create falls through as None rather
    /// than being mishandled — "addressed by owner" enforced by the same
    /// return type as "not my tool".
    #[test]
    fn an_outcome_from_a_child_i_did_not_create_is_not_mine() {
        let msg = Message::Child(ChildMsg::Outcome {
            child: RunnerId::new_v4(),
            outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                label: "x".into(),
                report: "y".into(),
            }),
        });
        assert!(cap().handle(AgentId::new_v4(), &msg).is_none());
    }

    /// The report goes to the agent that asked, read off `outstanding` — not
    /// off whichever agent happens to be current.
    #[test]
    fn a_report_is_delivered_to_the_agent_that_asked() {
        let asked_by = AgentId::new_v4();
        let child = RunnerId::new_v4();
        let mut c = cap();
        c.apply(&CapEvent::SubAgent(Event::Started { child, from: asked_by }));

        let msg = Message::Child(ChildMsg::Outcome {
            child,
            outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                label: "audit".into(),
                report: "three stale crates".into(),
            }),
        });
        let (events, actions) = c.handle(AgentId::new_v4(), &msg).expect("my child");
        assert!(matches!(&events[0], CapEvent::SubAgent(Event::Reported { child: c2 }) if *c2 == child));
        let Action::Deliver { to, part, .. } = &actions[0] else {
            panic!("expected Deliver");
        };
        assert_eq!(*to, asked_by);
        assert_eq!(part.text, "three stale crates");
    }

    /// Reporting clears the child, so a re-delivery after a crash stops once
    /// the acknowledgement is durable. `outstanding` is the single fact
    /// recording both "still owed" and "who to".
    #[test]
    fn reporting_clears_the_outstanding_child() {
        let asked_by = AgentId::new_v4();
        let child = RunnerId::new_v4();
        let mut c = cap();
        c.apply(&CapEvent::SubAgent(Event::Started { child, from: asked_by }));
        c.apply(&CapEvent::SubAgent(Event::Reported { child }));
        assert!(c.outstanding.is_empty());
    }

    /// Equipping is per agent: a capability built with a zero cap advertises
    /// no tool, so the model never meets one that only ever refuses.
    #[test]
    fn a_zero_cap_advertises_no_tool() {
        let mut settings = settings();
        settings.max_concurrent_subagents = Some(0);
        let mut spec = AgentSpec::default();
        SubAgentCapability::new(settings).setup(&mut spec);
        assert!(!spec.layers.iter().any(|l| matches!(l, ToolLayer::SpawnAgent { .. })));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server --lib sessions::runners::capabilities::sub_agent`
Expected: FAIL — `SubAgentCapability` not found.

- [ ] **Step 3: Write the implementation**

Follow the spec's worked handler. `Request` derives `Deserialize` so the tool schema and the handler input are one declaration. `Event::Started` inserts into `outstanding`; `Event::Reported` removes.

The child's `RunnerId` is generated in `handle`, not in `apply`: `handle` is a decision (allowed to be non-deterministic), `apply` is a fold (must not be). Put a comment saying so — this is the rule a later edit will otherwise break.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-server --lib sessions::runners::capabilities::sub_agent`
Expected: PASS, 5 tests.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo fmt --all
cargo clippy -p horsie-server --all-targets --all-features -- -D warnings
git add crates/server/src/sessions/runners/capabilities
git commit -m "feat(sessions): the subagent capability"
```

---

### Task A4: WorkflowCapability and ForkCapability

Same five-step shape as A3, one file each. Independent of A3 — the two may be written in parallel with it once A2 has landed.

**Files:**
- Create: `crates/server/src/sessions/runners/capabilities/workflow.rs`
- Create: `crates/server/src/sessions/runners/capabilities/fork.rs`
- Modify: `crates/server/src/sessions/runners/capabilities/mod.rs` — add both arms

**Interfaces:**
- Produces: `WorkflowCapability { outstanding: BTreeMap<RunnerId, AgentId> }`, `workflow::Event::{Started, Reported}`, `workflow::Request { workflow: String, input: String }`; `ForkCapability { pending: BTreeMap<RunnerId, AgentId> }`, `fork::Event::{Created, Seeded, SeedFailed}`, `fork::Request { mode: ForkMode, message: String }`.

Required tests:

- `invoke_workflow` emits `Started` plus `CreateChild { kind: RunnerKind::Workflow }`.
- A `ChildOutcome::Workflow(Finished { output })` for an owned child delivers the output to the asking agent and emits `Reported`.
- A `ChildOutcome::SubAgent(_)` is **not** `WorkflowCapability`'s, even for a child id it holds — the outcome kind and the owning capability must agree, and returning `None` is how they do.
- `ForkCapability` takes `Message::Command` for `/fork` and `/summary-n-fork` and rejects an empty message with no events and no actions.
- `ForkCapability` handles `ChildMsg::Ready` by clearing `pending`, and handles no `ChildMsg::Outcome` at all — a fork owes nobody a result.

---

### Task A5: the thin capabilities

**Files:**
- Create: `capabilities/ask_user.rs`, `capabilities/title.rs`, `capabilities/step_result.rs`
- Modify: `capabilities/mod.rs` — add three arms

**Interfaces:**
- Produces: `AskUserCapability { pending: Option<AgentId> }` (`Event::{Asked, Answered}`); `TitleCapability { title: Option<String> }` (`Event::Set { name: String }`); `StepResultCapability { outcomes: Vec<StepOutcome>, fields: Vec<StepField>, interactive: bool }` (`Event::Submitted { output: Value }`).

Required tests:

- `AskUserCapability::setup` pushes `ToolLayer::AskUser`; a capability constructed as unattended pushes nothing, and its `handle` of `Tool("ask_user")` still returns `None` so the tool cannot be reached by another route.
- `TitleCapability` takes `Tool("set_session_title")`, emits `Event::Set`, and its `apply` records the name.
- `StepResultCapability::setup` pushes `ToolLayer::SubmitResult` carrying the declared outcomes and fields, and pushes `ToolLayer::AskUser` only when `interactive`.
- `StepResultCapability` handling `Tool("submit_result")` emits `Event::Submitted` and no action — concluding the step is the runner's decision, not the capability's.

---

### Task A6: the namespace capabilities

**Files:**
- Create: `capabilities/runtime.rs`, `capabilities/mcp.rs`, `capabilities/memory.rs`, `capabilities/control_plane.rs`
- Modify: `capabilities/mod.rs` — add four arms and document the assembly order

**Interfaces:**
- Produces: `RuntimeCapability { accepts_all: bool }`, `McpCapability { servers: Vec<String> }`, `MemoryCapability { spaces: Vec<String> }`, `ControlPlaneCapability`. All four are stateless folds — their `Event` types are uninhabited (`enum Event {}`) because none of them records anything.

Required tests:

- `McpCapability` takes `Tool("mcp__github__list_issues")` and declines `Tool("bash")`.
- `ControlPlaneCapability` takes `Tool("horsie_sessions")` and declines `Tool("horsie")` — the prefix is `horsie_`, and a tool named exactly `horsie` is not in it.
- `MemoryCapability::setup` with no spaces pushes no layer, matching today's behaviour where a session naming no spaces is offered no memory tools.
- `RuntimeCapability` declines nothing, which is why assembly sorts it last; assert that `offer` with `[Runtime, Title]` in the wrong order routes `set_session_title` to `Runtime`, and that the documented order `[Title, Runtime]` routes it to `Title`. This test is the reason the order is written down.

---

### Task A7: SubAgentRunner and ConversationRunner

**Files:**
- Create: `crates/server/src/sessions/runners/subagent.rs`, `crates/server/src/sessions/runners/conversation.rs`
- Modify: `crates/server/src/sessions/runners/mod.rs` — the `Runner` and `AgentLifecycle` traits, and the real `RunnerState` arms

**Interfaces:**
- Produces:

```rust
pub type Emit = (Vec<RunnerEvent>, Vec<Action>);

pub trait Runner {
    fn actions(&self, s: &Self::State, view: &SessionView<'_>) -> Vec<Action>;
    fn outcome(s: &Self::State) -> Option<ChildOutcome>;
    fn busy(s: &Self::State) -> bool;
    fn apply(s: &mut Self::State, e: &Self::Event);
    fn capabilities(s: &Self::State) -> &[Capability];
}

pub trait AgentLifecycle {
    fn on_agent_started(&self, s: &Self::State, agent: AgentId) -> Emit;
    fn on_agent_ended(&self, s: &Self::State, agent: AgentId, end: TurnEnd) -> Emit;
    fn on_agent_halted(&self, s: &Self::State, agent: AgentId, reason: String) -> Emit;
}
```

Required tests:

- A `Pending` `SubAgentRunner` with no agent returns exactly one `Action::StartAgent`; the same state after the `Started` event returns none. This is the idempotence that lets creation and recovery share a path, so it is the most important test in Phase A.
- `SubAgentRunner::outcome` is `None` while running, and `Some(ChildOutcome::SubAgent(Completed { .. }))` once concluded.
- `SubAgentRunner::on_agent_ended(TurnEnd::Asked)` records a failure — a subagent has no ask tool, so this is real behaviour, and it is the translation the parent must never see.
- `ConversationRunner::outcome` is `None` in every state, including terminal ones. That is what lets `parent` mean provenance rather than debt, and what makes a fork's "reports nothing" structural.
- A `ConversationRunner` built with a `seed` returns `StartAgent` only after `Event::Seeded`; without the seed it returns nothing, so a fork cannot run before its branch point is durable.

---

### Task A8: WorkflowRunner and RuntimeRunner

**Files:**
- Create: `crates/server/src/sessions/runners/workflow.rs`, `crates/server/src/sessions/runners/runtime.rs`
- Modify: `crates/server/src/sessions/runners/mod.rs`

**Interfaces:**
- Produces: `workflow::State { graph: Arc<WorkflowRunSpec>, steps: Vec<StepRun>, output, error, usage, capabilities }`; `runtime::State { provisioned_at_ms, phase, detail }`. `WorkflowRunner` implements both `Runner` and `AgentLifecycle`; `RuntimeRunner` implements `Runner` only.

Required tests:

- `WorkflowRunner::actions` on a fresh state starts the graph's start step; with a step in flight it starts nothing.
- A concluded step routes through `next_transition` to the next step, reusing `crate::sessions::workflow::WorkflowOrchestrator`'s existing decision logic rather than a second copy.
- `WorkflowRunner::outcome` is `Some(Workflow(Finished { .. }))` only once the run itself is terminal, never per step — the difference between a step's result and the run's.
- `RuntimeRunner` does not implement `AgentLifecycle`. Assert by absence: a doc test or a compile-fail note is overkill, so instead assert `RuntimeRunner::capabilities(&s).is_empty()` and state the reason in a comment.
- A workflow whose `graph` came from the runner's own state, not from `SessionSpec` — build the state with an inline `WorkflowRunSpec` and assert the run starts. This is the test that proves an ad-hoc graph will work.

---

### Task A9: assembly, the fold, and the module's public face

**Files:**
- Modify: `crates/server/src/sessions/runners/mod.rs` — `RunnerEvent`, `RunnerState`, `SessionEvent`, and the fold
- Modify: `crates/server/src/sessions/runners/state.rs` — `SessionState::apply`

**Interfaces:**
- Produces: `SessionEvent::{RunnerCreated, RunnerEnded, Runner(RunnerId, RunnerEvent), UsageBanked, SpecRecorded}`; `SessionState::apply(&mut self, &SessionEvent)`; `assemble(kind, args) -> Vec<Capability>`.

Required tests:

- `RunnerCreated` inserts a record with `status: Pending` and registers nothing in `agents`; the runner's first `StartAgent` is what adds the agent, so a crash between them replays as a runner with no agent, which `actions()` restarts.
- Folding a sequence twice from `default()` produces byte-identical state (serialize both and compare). This is the recovery contract, and a fold that reached for a clock or a random id fails it.
- `assemble(RunnerKind::Workflow, ..)` returns capabilities with `RuntimeCapability` last, and `assemble(RunnerKind::Runtime, ..)` returns none.
- An event addressed to an unknown `RunnerId` is ignored rather than panicking — a fold must survive a log it does not fully understand.

- [ ] **Final step: PR 1**

```bash
cargo fmt --all
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test -p horsie-server --lib sessions::runners
git push -u origin design/session-runners
gh pr create --fill-first
```

PR body follows `.github/pull_request_template.md`: **Why** the four-kind vocabulary blocks agent-invoked workflows; **What** the runner/capability core, unwired; **Verification** the unit suite plus the fold-twice recovery test; **Docs** the spec and this plan.

---

## Phase B — wire it in (PR 2)

Phase B is where the old session actor is replaced. It is one PR because the state shape changes: there is no intermediate commit in which half the components read `state.run` and half read `state.runners`.

**These tasks are scoped, not yet decomposed.** Phase A's tasks carry the code an executor needs; B's and C's carry the boundary, the files and the acceptance test. Re-run the writing-plans skill against this section before executing it, once Phase A has landed and the real signatures exist to write against. Decomposing them now would mean inventing signatures Phase A has not settled.

### Blast radius (surveyed 2026-08-15)

The session command vocabulary never escapes `crates/server/src/sessions/`. Nothing in `http/`, `control/`, `routines/` or `crates/tests/` names `SessionCommand` — they all go through `SessionSupervisorCommand`, which the supervisor translates. That makes Phase B a within-module rewrite plus a handful of edges:

- `sessions/addressing.rs` — `SessionInbox = Addressed<SessionEntityId, SessionCommand>` and the `tell`/`ask` signatures. The one structural dependency.
- `sessions/supervisor.rs` — the single production sender: 17 call sites across `Lifecycle`, `Turn`, `Run`, `Read`, `Fork`, `Core`. Also holds `SessionSnapshot`, `SessionUsageStats`, `AgentDetail`, `AgentStatus` in reply types.
- `sessions/lifecycle_routing.rs` — the heaviest consumer, and the one to rewrite first: `route(&SessionDomainEvent, &SessionState) -> Vec<(AgentKey, LifecycleEvent)>` matches nearly every event variant, and its tests are deliberately exhaustive so a new variant breaks the build.
- `sessions/orchestrator.rs` — `owed_deliveries` and the `SubAgentParent`/`TreeOwner` → `AgentKey` mapping. Deleted; the capability owns delivery now.
- `sessions/spawn_tool.rs`, `sessions/title_tool.rs` — the two in-session tools that send commands. They become capability request types.
- `sessions/workflow/driver.rs` — reaches into `state.run` directly; rebind to the runner's own slice.
- `sessions/forks.rs` — borrows `AgentStatus` for its roster; deleted with the roster.
- `http/handlers.rs` — the only HTTP file naming actor types: `AgentEntry`, `SessionSnapshot`, `MAIN_AGENT_ID`, and `to_wire_agent`.
- `sessions/events.rs` — `fold_session_state`/`fold_agent_state` are `#[cfg(test)]` helpers used throughout the session_actor tests; they fold with `SessionActor::apply_event` and must be rebound to `SessionState::apply`.

**Keep the re-export.** `session_actor/types.rs` re-exports `AskAnswer` and `AnswerError` from `crate::agent_loop`, and both `supervisor.rs` and `http/handlers.rs` import them *through* session_actor. Dropping the re-export breaks two call sites for types that did not change.

### What reading the real toolboxes changed

Phase A modelled an agent's equipment as `ToolLayer` — a *name* for a toolbox to wrap. Reading the six bespoke wrappers shows that name only fits half of them, and the split matters enough to fix before the actor depends on it.

`SubAgentToolbox`, `AskUserToolbox`, `SessionTitleToolbox` and `StepResultToolbox` all do the same three things: advertise a `ToolSpec`, match one tool name, and send a typed `SessionCommand` awaiting a reply. Under capabilities that dispatch is uniform — the session routes a tool call to the owning runner and offers it around — so all four collapse into **one** `SessionToolbox { specs, session, agent }` that forwards `(agent, name, input)` and renders the `Action::Reply` back. Four files become one, and a new session-routed tool becomes a `ToolSpec` on a capability rather than a new wrapper.

`RuntimeCapability`, `McpCapability`, `MemoryCapability` and `ControlPlaneCapability` are different in kind: they execute against real services, never through the mailbox. `ToolLayer` names those correctly and they keep it.

So `AgentSpec` carries both — `layers: Vec<ToolLayer>` for the service-backed wrappers, and `tools: Vec<ToolSpec>` for everything the session routes — and a capability's `setup` pushes whichever it owns. This also puts a tool's schema next to the handler that answers it, which is the last place the two could drift.

### Superseded: the catalogue question

An earlier draft of this section agonised over where `spawn_agent`'s installed-types list could come from, and offered three options. All three were answers to a problem that only existed because `Capability::setup` was synchronous and therefore had to run on the mailbox, where no scan has happened.

With `setup` async and on the agent's own task, `SubAgentCapability::setup` reads the catalogue directly — the runtime capability put it in the spec a moment earlier — and keeps it for `handle` to validate against. There is nothing to decide.

### Task B1: `AgentSpec` carries real tool specs

**Files:** `runners/action.rs` (add `tools: Vec<ToolSpec>`), the six session-routed capabilities (`setup` pushes its real `ToolSpec`, moved from the wrapper that owns it today), `sessions/spawn_tool.rs`/`ask_tool.rs`/`title_tool.rs`/`workflow/toolbox.rs` (the spec builders become `pub` so the capability can call them; the wrappers stay until B3).

**Test:** a conversation runner's assembled capabilities produce a spec whose `tools` name `spawn_agent`, `subagent_status`, `invoke_workflow`, `workflow_status`, `ask_user`, `set_session_title` — and a subagent runner's name only the first four. Green on its own; nothing is wired yet.

### Task B2: `SessionToolbox`, the one forwarding wrapper

**Files:** create `sessions/session_toolbox.rs`.

`Toolbox::specs` returns the inner toolbox's plus `self.specs`; `execute` forwards any name in `self.specs` to the session as `SessionCommand::AgentTool { agent, call, reply }` and passes everything else inward.

**Test:** a call to a forwarded name reaches a recording fake session with the agent id attached; an unforwarded name reaches the inner toolbox. This is the seam the whole rewrite rests on, so it is tested before anything depends on it.

### Task B3: the equipment builder

**Files:** create `sessions/equipment.rs`.

One function turning an `AgentSpec` into `(Arc<dyn Toolbox>, Option<String>)`: fold `layers` over the base toolbox in order (`Runtime` → `Memory` → `ControlPlane` → `Mcp`), then wrap once in `SessionToolbox` carrying `tools`, then join the `PromptSection`s onto the composed system prompt. It needs the runtime client, the session ref, the agent id and the account's services — take them in a struct rather than as eight arguments.

**Test:** a spec with `[Runtime, Memory{spaces}]` plus `tools: [ask_user]` advertises the sandbox tools, the five memory tools and `ask_user`; the same spec without the memory layer advertises neither the memory tools nor a stub that refuses. This replaces `context.rs`'s two four-arm matches, so assert against tool *names* rather than against layer identity.

### Task B4: `SessionActor` dispatches to runners

The big one, and the only task in this phase that cannot be green on its own.

- `handle_command` resolves `agents[id] -> runner` and routes; `SessionCommand` shrinks to `AgentTool`, `UserMessage`, `Stop`, `Answer`, `Read`, `Lifecycle`, `Core`, and `AgentOutcome`.
- `apply_event` folds through `SessionState::apply`; `on_events_persisted` reports `state.status()`.
- The boundary performs `Action`s and re-drives: persist, fold, then for every `Done` runner with a parent, offer its `outcome()` to the parent's capabilities; then collect `actions()` from every runner.
- `Action` gains the two arms Phase A named as missing: one for the runtime declaring itself provisioned, one for a runner ending itself.
- `Component` and `session_actor/{turns,run,subagent,fork,lifecycle,core}.rs` are deleted; `reads.rs` and `hooks.rs` are rewritten against the new state.
- `lifecycle_routing.rs` — the heaviest consumer, with deliberately exhaustive tests — is rewritten to fan out from `SessionEvent` instead of `SessionDomainEvent`.

**Test:** the existing `session_actor` integration tests, ported. A green port is the acceptance criterion — it is what says the rewrite preserved behaviour rather than merely compiling.

### Task B5: delete the old vocabulary

`subagents.rs`'s forest (`SubAgentForest`, `SubAgentTree`, `TreeOwner`, `owner_for`, `root_owner`), `forks.rs`, `orchestrator.rs`, `AgentKey`, `SessionAgentKind`, `effective_settings`, `effective_settings_for_parent`, and the four bespoke tool wrappers B2 replaced. Update `http/handlers.rs`'s `AgentEntry`/`SessionSnapshot`/`to_wire_agent` and `supervisor.rs`'s reply types.

**Keep the re-export**: `session_actor/types.rs` re-exports `AskAnswer` and `AnswerError` from `crate::agent_loop`, and `supervisor.rs` and `http/handlers.rs` import them *through* it. Dropping it breaks two call sites for types that did not change.

**Test:** `cargo clippy --all-targets -- -D warnings` is the test — a dead type is a warning, and the deletion is complete when nothing names the old vocabulary.

### Task B6: enforce invariant 6

An agent may not conclude while it has outstanding children. `StepResultCapability::handle` refuses `submit_result`, and `ConversationRunner::on_agent_ended` defers the boundary, while the runner's `SubAgentCapability`/`WorkflowCapability` still hold outstanding children.

**Test:** a step that submits with a subagent still running does not conclude; when the child reports, it concludes on its next turn. **Write it first and watch it fail against B4's code** — it is the invariant that licenses a single `SubAgentCapability` implementation for all three parent kinds, so a test that passes immediately has proved nothing.

### Task B7: the cancel cascade, and the session-wide cap

Cancelling a runner cancels the runners parented on its agents, recursively, over `SessionState::children_of`. And the concurrency cap moves to the session: the count is global — how many agents may run at once on one sandbox — and is checked before a delegation is dispatched, beside "does this agent have this tool". `AgentSettings::max_concurrent_subagents` stops being the enforced number and remains only what a capability advertises.

**Tests:** cancelling a workflow step with a subagent in flight leaves no runner `Running`; with the session cap reached, `spawn_agent` from any runner is refused with a message naming the cap and journals no `RunnerCreated`.


## Phase C — invoke_workflow (PR 3)

### Task C1: the tool

`WorkflowCapability::setup` pushes `ToolLayer::InvokeWorkflow`; `context.rs` builds the toolbox layer; the tool's arguments deserialize into `workflow::Request`. Resolving a workflow name to a graph is async and must not run on the session mailbox — it happens on a detached task that self-sends the resolved graph, the shape `ForkCommand::Seeded` already uses.

Test: an agent calling `invoke_workflow` gets a runner id back only after the create is durable; the run's terminal output arrives in the caller's queue as a report.

### Task C2: the recursion budget

One depth number across the combined runner tree, replacing `MAX_SUBAGENT_DEPTH`, checked in `SubAgentCapability::handle` and `WorkflowCapability::handle` against `SessionView::depth`.

Test: a workflow whose step invokes the same workflow stops at the budget with an error the model can read, rather than growing until the session dies.

### Task C3: docs

`docs/` gains a page on invoking a workflow from an agent, and the workflows guide gains the nesting rules. Both in the same commit as the behaviour, per the PR template's Docs checkbox.
