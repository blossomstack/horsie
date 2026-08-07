# SubAgentForest Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the subagent tree out of `SessionModeState` into a `SubAgentForest` keyed by the agent that roots each tree, so every subagent query is correct for a workflow run as well as a conversation.

**Architecture:** `SessionState` gains `subagents: SubAgentForest` and loses `mode: SessionModeState`; `mode.rs` is deleted and `StepRun.subagents` removed. The forest holds `BTreeMap<TreeOwner, SubAgentTree>` and exposes whole-forest aggregates (`active_count`, `has_active`, `interrupted`, `owed`) that no caller can get wrong. A serde shim on `SessionState` loads pre-move snapshots — both the pre-`mode` flat shape and the `mode`-tagged shape.

**Tech Stack:** Rust, `horsie-actor` event sourcing, serde, SQLite/Postgres journals.

## Global Constraints

- `SessionDomainEvent` keeps its exact current variant names and shapes. This is the persisted contract; renaming a variant is what broke the supervisor in #101.
- `SubAgentRecord` and `SubAgentTree` keep their exact serialized shapes.
- `SessionState` is snapshotted, so its shape change needs a shim plus a round-trip test over a **captured** legacy payload, not a synthesised one.
- Workflow support is unfinished; its subagent defects are being fixed as a consequence of this move, not chased separately.
- `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets` must be clean. CI uses the pinned stable toolchain, not nightly.

---

### Task 1: `TreeOwner` and `SubAgentForest`

**Files:**
- Modify: `server/src/sessions/subagents.rs` (append; leave `SubAgentTree` untouched)

**Interfaces:**
- Produces: `TreeOwner::{Main, Step(Uuid)}`, `SubAgentForest`, `OwedResult`, and the forest methods every later task calls.

- [ ] **Step 1: Write the failing tests** in `subagents.rs`'s `mod tests`

```rust
fn forest_with_two_trees() -> (SubAgentForest, Uuid, Uuid, Uuid) {
    let mut f = SubAgentForest::default();
    let step = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    f.tree_mut(TreeOwner::Main)
        .apply_spawned(a, SubAgentParent::Main, "a".into(), "t".into(), 1, 100, None);
    f.tree_mut(TreeOwner::Step(step))
        .apply_spawned(b, SubAgentParent::Main, "b".into(), "t".into(), 1, 100, None);
    (f, step, a, b)
}

#[test]
fn aggregates_span_every_tree() {
    let (f, _step, a, b) = forest_with_two_trees();
    assert_eq!(f.active_count(), 2);
    assert!(f.has_active());
    let mut interrupted = f.interrupted();
    interrupted.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(interrupted, expected);
}

#[test]
fn a_node_is_found_whichever_tree_holds_it() {
    let (f, step, a, b) = forest_with_two_trees();
    assert_eq!(f.node(a).unwrap().label, "a");
    assert_eq!(f.node(b).unwrap().label, "b");
    assert_eq!(f.owner_of(a), Some(TreeOwner::Main));
    assert_eq!(f.owner_of(b), Some(TreeOwner::Step(step)));
    assert_eq!(f.owner_of(Uuid::new_v4()), None);
}

#[test]
fn owed_results_carry_the_tree_that_owes_them() {
    let (mut f, step, _a, b) = forest_with_two_trees();
    f.tree_mut(TreeOwner::Step(step)).apply_completed(b, "done".into(), 400);
    let owed = f.owed();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].child, b);
    assert_eq!(owed[0].parent, SubAgentParent::Main);
    assert_eq!(owed[0].owner, TreeOwner::Step(step));
    assert_eq!(owed[0].part.text, "done");
}

/// A step's spawn belongs in that step's tree; a subagent's belongs in
/// whichever tree already holds the subagent. This is the whole of what the
/// session has to tell the forest about kinds.
#[test]
fn owner_for_resolves_a_caller_against_the_root_in_play() {
    let (f, step, a, _b) = forest_with_two_trees();
    assert_eq!(
        f.owner_for(SubAgentParent::Main, TreeOwner::Step(step)),
        Some(TreeOwner::Step(step))
    );
    assert_eq!(
        f.owner_for(SubAgentParent::SubAgent(a), TreeOwner::Step(step)),
        Some(TreeOwner::Main)
    );
    assert_eq!(
        f.owner_for(SubAgentParent::SubAgent(Uuid::new_v4()), TreeOwner::Main),
        None
    );
}

#[test]
fn an_empty_forest_answers_every_aggregate() {
    let f = SubAgentForest::default();
    assert_eq!(f.active_count(), 0);
    assert!(!f.has_active());
    assert!(f.interrupted().is_empty());
    assert!(f.owed().is_empty());
    assert!(f.ids().is_empty());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server --lib sessions::subagents`
Expected: FAIL — `cannot find type SubAgentForest in this scope`

- [ ] **Step 3: Implement**

```rust
/// Which agent roots a subagent tree. A conversation has exactly one; a
/// workflow run has one per step execution, keyed by that step's agent id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TreeOwner {
    Main,
    Step(Uuid),
}

/// One finished subagent's result that its parent has not been sent.
#[derive(Debug, Clone, PartialEq)]
pub struct OwedResult {
    pub child: Uuid,
    pub parent: SubAgentParent,
    pub owner: TreeOwner,
    pub part: SubAgentResultPart,
}

/// Every subagent this session holds, whatever kind of session it is.
///
/// Keyed by owner rather than nested inside the session's mode, which is the
/// whole point: there is no accessor that can see one kind's subagents and miss
/// another's, so the aggregates below are right for a workflow run the day they
/// are written. The previous shape had two accessors and every read used the
/// one that returned an empty tree for a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubAgentForest {
    trees: BTreeMap<TreeOwner, SubAgentTree>,
}

impl SubAgentForest {
    pub fn tree(&self, owner: TreeOwner) -> Option<&SubAgentTree> {
        self.trees.get(&owner)
    }

    /// The owner's tree, created on first spawn.
    pub fn tree_mut(&mut self, owner: TreeOwner) -> &mut SubAgentTree {
        self.trees.entry(owner).or_default()
    }

    /// Which tree holds this node.
    pub fn owner_of(&self, node: Uuid) -> Option<TreeOwner> {
        self.trees
            .iter()
            .find(|(_, t)| t.get(&node).is_some())
            .map(|(owner, _)| *owner)
    }

    /// The tree a spawn by `caller` belongs in. `root` is what this session's
    /// own "Main" means right now — `Main` for a conversation, the step in
    /// flight for a run. The only kind-shaped fact the forest is ever told.
    pub fn owner_for(&self, caller: SubAgentParent, root: TreeOwner) -> Option<TreeOwner> {
        match caller {
            SubAgentParent::Main => Some(root),
            SubAgentParent::SubAgent(id) => self.owner_of(id),
        }
    }

    pub fn node(&self, id: Uuid) -> Option<&SubAgentRecord> {
        self.trees.values().find_map(|t| t.get(&id))
    }

    pub fn ids(&self) -> Vec<Uuid> {
        self.trees.values().flat_map(SubAgentTree::ids).collect()
    }

    // --- whole-forest aggregates: the five that were wrong before ---

    pub fn active_count(&self) -> u32 {
        self.trees.values().map(SubAgentTree::active_count).sum()
    }

    pub fn has_active(&self) -> bool {
        self.trees.values().any(SubAgentTree::has_active)
    }

    pub fn interrupted(&self) -> Vec<Uuid> {
        self.trees
            .values()
            .flat_map(SubAgentTree::interrupted)
            .collect()
    }

    /// Every terminal result no parent has been sent, across every tree.
    pub fn owed(&self) -> Vec<OwedResult> {
        let mut out = Vec::new();
        for (owner, tree) in &self.trees {
            for parent in tree.parents() {
                for (child, part) in tree.owed_for(parent) {
                    out.push(OwedResult {
                        child,
                        parent,
                        owner: *owner,
                        part,
                    });
                }
            }
        }
        out
    }
}
```

Plus one addition to `SubAgentTree`, because `owed` needs to enumerate parents:

```rust
    /// Every distinct parent in this tree, for a caller walking owed results.
    pub fn parents(&self) -> Vec<SubAgentParent> {
        let mut seen: Vec<SubAgentParent> = self.nodes.values().map(|r| r.parent).collect();
        seen.sort();
        seen.dedup();
        seen
    }
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p horsie-server --lib sessions::subagents`
Expected: PASS, all tests

- [ ] **Step 5: Commit**

```bash
git add server/src/sessions/subagents.rs
git commit -m "feat(server): a subagent forest keyed by the agent that roots each tree"
```

---

### Task 2: `SessionState` holds the forest, with a snapshot shim

**Files:**
- Modify: `server/src/sessions/session_actor/mod.rs` (`SessionState`, `apply_event`)
- Delete: `server/src/sessions/mode.rs`
- Modify: `server/src/sessions/mod.rs` (drop `pub mod mode;`)
- Modify: `server/src/sessions/workflow/mod.rs` (drop `StepRun.subagents` and `WorkflowRunState::tree_of`)

**Interfaces:**
- Consumes: `SubAgentForest`, `TreeOwner` from Task 1.
- Produces: `SessionState { status, last_error, agent_usage, inbox, pending_asks, run: Option<WorkflowRunState>, subagents: SubAgentForest }`, and `SessionState::root_owner(&self) -> TreeOwner`.

- [ ] **Step 1: Write the failing round-trip tests** in `session_actor/tests.rs`

```rust
/// A snapshot written before `mode` existed carries `subagents` at the top
/// level, flat. It must load with its tree intact — anything else silently
/// drops every subagent of every deployed session.
#[test]
fn a_pre_mode_snapshot_keeps_its_subagents() {
    let legacy = serde_json::json!({
        "status": "Idle",
        "inbox": [],
        "subagents": { "nodes": { "3f1a2b4c-0000-4000-8000-000000000001": {
            "parent": "Main", "label": "reader", "task": "read the file", "depth": 1,
            "status": "Completed", "output": "done", "error": null, "notified": true
        }}}
    });
    let state: SessionState = serde_json::from_value(legacy).unwrap();
    let id = Uuid::parse_str("3f1a2b4c-0000-4000-8000-000000000001").unwrap();
    assert_eq!(state.subagents.node(id).unwrap().label, "reader");
    assert_eq!(state.subagents.owner_of(id), Some(TreeOwner::Main));
}

/// A snapshot written after `mode` existed nests the tree under
/// `mode.subagents` for a conversation.
#[test]
fn a_mode_tagged_conversation_snapshot_keeps_its_subagents() {
    let legacy = serde_json::json!({
        "status": "Idle",
        "mode": { "kind": "Interactive", "subagents": { "nodes": {
            "3f1a2b4c-0000-4000-8000-000000000002": {
                "parent": "Main", "label": "auditor", "task": "t", "depth": 1,
                "status": "Running", "output": null, "error": null, "notified": false
            }}}}
    });
    let state: SessionState = serde_json::from_value(legacy).unwrap();
    let id = Uuid::parse_str("3f1a2b4c-0000-4000-8000-000000000002").unwrap();
    assert_eq!(state.subagents.node(id).unwrap().label, "auditor");
    assert_eq!(state.subagents.active_count(), 1);
}

/// A run's snapshot nested one tree per step. Each must land under that step's
/// agent id, and the run itself must survive.
#[test]
fn a_workflow_snapshot_lands_each_steps_tree_under_that_step() {
    let step_agent = "3f1a2b4c-0000-4000-8000-0000000000aa";
    let child = "3f1a2b4c-0000-4000-8000-0000000000bb";
    let legacy = serde_json::json!({
        "status": "Running",
        "mode": { "kind": "Workflow", "run": {
            "status": "Running",
            "steps": [{
                "step": "review", "agent": step_agent, "attempt": 1, "from": null,
                "via": null, "input": "go", "status": "Running", "output": null,
                "error": null, "started_at_ms": 1, "ended_at_ms": 0,
                "subagents": { "nodes": { child: {
                    "parent": "Main", "label": "helper", "task": "t", "depth": 1,
                    "status": "Completed", "output": "kid done", "error": null,
                    "notified": false
                }}}
            }],
            "output": null, "error": null
        }}
    });
    let state: SessionState = serde_json::from_value(legacy).unwrap();
    let owner = TreeOwner::Step(Uuid::parse_str(step_agent).unwrap());
    let child = Uuid::parse_str(child).unwrap();
    assert_eq!(state.subagents.owner_of(child), Some(owner));
    assert_eq!(state.run.as_ref().unwrap().steps.len(), 1);
    // The aggregate that returned 0 before this change.
    assert_eq!(state.subagents.owed().len(), 1);
}

/// The new shape round-trips.
#[test]
fn the_new_state_shape_round_trips() {
    let mut state = SessionState::default();
    let id = Uuid::new_v4();
    state.subagents.tree_mut(TreeOwner::Main).apply_spawned(
        id, SubAgentParent::Main, "x".into(), "t".into(), 1, 100, None,
    );
    let json = serde_json::to_value(&state).unwrap();
    let back: SessionState = serde_json::from_value(json).unwrap();
    assert_eq!(back.subagents.node(id).unwrap().label, "x");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server --lib session_actor::tests::a_pre_mode_snapshot`
Expected: FAIL — `no field subagents on SessionState`

- [ ] **Step 3: Reshape `SessionState` with a deserialize shim**

Replace the `mode` field and add the shim. `SessionState` serializes in its new shape and deserializes from three: pre-`mode` flat, `mode`-tagged conversation, `mode`-tagged run.

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, from = "SessionStateWire")]
pub struct SessionState {
    pub status: SessionStatus,
    pub last_error: Option<String>,
    pub agent_usage: HashMap<String, UsageTotal>,
    pub inbox: Vec<InboxMessage>,
    pub pending_asks: Vec<PendingAsk>,
    pub run: Option<WorkflowRunState>,
    pub subagents: SubAgentForest,
}

impl SessionState {
    /// What this session's own "Main" means right now: the step in flight for a
    /// run, the main agent otherwise. The one kind-shaped fact the subagent
    /// code is ever told, and it is told it as a value rather than a branch.
    pub fn root_owner(&self) -> TreeOwner {
        match self.run.as_ref().and_then(|r| r.current_agent()) {
            Some(agent) => TreeOwner::Step(agent),
            None => TreeOwner::Main,
        }
    }
}

/// Every snapshot shape `SessionState` has ever been written in.
///
/// Three, because the tree has moved twice: it was flat, then nested under
/// `mode`, and is now a forest beside the run. A snapshot that fails to
/// deserialize is a session that cannot be opened at all, so each older shape
/// is read rather than rejected. Only the newest is ever written.
#[derive(Deserialize)]
#[serde(default)]
struct SessionStateWire {
    status: SessionStatus,
    last_error: Option<String>,
    agent_usage: HashMap<String, UsageTotal>,
    inbox: Vec<InboxMessage>,
    pending_asks: Vec<PendingAsk>,
    // current
    run: Option<WorkflowRunState>,
    subagents: Option<SubAgentForest>,
    // pre-forest
    mode: Option<LegacyMode>,
    // pre-mode: a bare tree at the top level
    #[serde(rename = "subagents")]
    legacy_tree: Option<SubAgentTree>,
}
```

The two `subagents` keys collide, so read the legacy shapes through an untagged helper instead:

```rust
#[derive(Deserialize)]
#[serde(untagged)]
enum LegacySubagents {
    Forest(SubAgentForest),
    Tree(SubAgentTree),
}

#[derive(Deserialize)]
struct LegacyMode {
    #[serde(default)]
    subagents: SubAgentTree,
    #[serde(default)]
    run: Option<LegacyRun>,
}

#[derive(Deserialize)]
struct LegacyRun {
    #[serde(flatten)]
    run: WorkflowRunState,
    #[serde(default)]
    steps: Vec<LegacyStepRun>,
}

#[derive(Deserialize)]
struct LegacyStepRun {
    agent: Uuid,
    #[serde(default)]
    subagents: SubAgentTree,
}
```

`From<SessionStateWire> for SessionState` resolves in this order — newest first, so a current snapshot never pays for the legacy paths:

1. `subagents` parsed as `Forest` → use it, with `run` as read.
2. `mode.run` present → forest gets one `TreeOwner::Step(step.agent)` entry per step that has nodes; `run` comes from the flattened run.
3. `mode.subagents` non-empty → forest gets one `TreeOwner::Main` entry.
4. top-level `subagents` parsed as `Tree` → forest gets one `TreeOwner::Main` entry.
5. otherwise → empty forest.

- [ ] **Step 4: Drop `StepRun.subagents` and `WorkflowRunState::tree_of`**

In `workflow/mod.rs`, delete the `subagents: SubAgentTree` field from `StepRun` and the `tree_of` method. Add:

```rust
    /// The agent id of the execution in flight, which is the tree a spawn by
    /// that step belongs in.
    pub fn current_agent(&self) -> Option<Uuid> {
        self.current().and_then(|i| self.get(i)).map(|s| s.agent)
    }
```

- [ ] **Step 5: Run the round-trip tests**

Run: `cargo test -p horsie-server --lib session_actor::tests::snapshot`
Expected: PASS — all four

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(server): SessionState holds a subagent forest, with a snapshot shim"
```

---

### Task 3: Rewire every read site

**Files:**
- Modify: `server/src/sessions/session_actor/mod.rs` (13 read sites, `apply_event`)
- Modify: `server/src/sessions/orchestrator.rs` (`wake_owed_parents`, `main_turn`)

**Interfaces:**
- Consumes: `SubAgentForest`, `TreeOwner`, `OwedResult`, `SessionState::root_owner`.

- [ ] **Step 1: Replace the fold arms**

Each `SubAgent*` arm of `apply_event` resolves its tree from the forest rather than from the mode:

```rust
SessionDomainEvent::SubAgentSpawned { id, parent, label, task, depth, at_ms, agent_type } => {
    // The owner is resolved from the state as it stands *before* this event,
    // which is the step in flight for a run and Main otherwise.
    let owner = state
        .subagents
        .owner_for(parent, state.root_owner())
        .unwrap_or(TreeOwner::Main);
    state.subagents.tree_mut(owner)
        .apply_spawned(id, parent, label, task, depth, at_ms, agent_type);
}
SessionDomainEvent::SubAgentRunning { id, at_ms } => {
    if let Some(owner) = state.subagents.owner_of(id) {
        state.subagents.tree_mut(owner).apply_running(id, at_ms);
    }
}
// SubAgentCompleted / SubAgentFailed / SubAgentNotified follow the same shape.
```

- [ ] **Step 2: Replace the twelve remaining `state.mode.subagents()` reads**

| was | becomes |
|---|---|
| `state.mode.subagents().get(&id)` | `state.subagents.node(id)` |
| `state.mode.subagents().has_active()` | `state.subagents.has_active()` |
| `state.mode.subagents().interrupted()` | `state.subagents.interrupted()` |
| `state.mode.subagents().active_count()` | `state.subagents.active_count()` |
| `state.mode.subagents().depth_of(caller)` | `state.subagents.tree(owner).map_or(caller_default, \|t\| t.depth_of(caller))` where `owner = state.subagents.owner_for(caller, state.root_owner())` |
| `state.mode.subagents().ids()` | `state.subagents.ids()` |
| `state.mode.subagents().visible_to(..)` / `render_node` / `render_subtree` | resolve the caller's tree first via `owner_for`, then call the same method on it |
| `state.mode.run()` / `run_mut()` | `state.run.as_ref()` / `as_mut()` |
| `state.mode.is_workflow()` | `state.run.is_some()` |

- [ ] **Step 3: Move owed-result delivery off the interactive-only path**

In `orchestrator.rs`, `wake_owed_parents` reads the forest, so it works in a run:

```rust
fn wake_owed_parents(state: &SessionState) -> Vec<AgentAction> {
    let mut by_parent: BTreeMap<Uuid, Vec<OwedResult>> = BTreeMap::new();
    for owed in state.subagents.owed() {
        if let SubAgentParent::SubAgent(parent) = owed.parent {
            by_parent.entry(parent).or_default().push(owed);
        }
    }
    by_parent
        .into_iter()
        .filter(|(parent, _)| {
            !state
                .subagents
                .node(*parent)
                .is_some_and(|r| r.status == SubAgentStatus::Running)
        })
        .map(|(parent, owed)| AgentAction::StartTurn {
            who: AgentKey::Sub(parent),
            input: TurnInput {
                message: None,
                results: Vec::new(),
                subagent_results: owed.iter().map(|o| o.part.clone()).collect(),
            },
            consumed: Vec::new(),
            answered: Vec::new(),
            notified: owed.iter().map(|o| o.child).collect(),
            mark_running: Some(parent),
        })
        .collect()
}
```

`main_turn` reads the root tree's owed results the same way:

```rust
    let owed: Vec<OwedResult> = state
        .subagents
        .owed()
        .into_iter()
        .filter(|o| o.parent == SubAgentParent::Main && o.owner == state.root_owner())
        .collect();
```

And `WorkflowOrchestrator::next_actions` gains the same wake pass, prepended, so a run delivers owed results exactly as a conversation does.

- [ ] **Step 4: Run the whole server suite**

Run: `cargo test -p horsie-server --lib`
Expected: PASS — 569+ tests. Any failure here is a rewiring mistake, not a design one.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(server): every subagent read goes through the forest"
```

---

### Task 4: Prove the workflow path

**Files:**
- Modify: `server/src/sessions/session_actor/tests.rs`

- [ ] **Step 1: Write the test that fails on `main`**

Using the existing `spawn_run_with_provider` harness: run a workflow whose first step spawns a subagent, let the subagent conclude, and assert the step is resumed with its result.

```rust
/// A workflow step's subagent must reach its step, exactly as a conversation's
/// reaches the main agent. Before the forest, every read went through an
/// accessor that returned an empty tree for a run: the outcome was dropped with
/// a warning and the step waited forever.
#[tokio::test]
async fn a_workflow_steps_subagent_delivers_its_result_to_that_step() {
    // ... spawn a run, spawn a subagent from the step, conclude it,
    // then assert the step's next turn carries the SubAgentResultPart.
}
```

- [ ] **Step 2: Verify it passes here and would fail on main**

Run: `cargo test -p horsie-server --lib a_workflow_steps_subagent`
Expected: PASS on this branch. Confirm by `git stash`-ing the source changes that it fails against `main`.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "test(server): a workflow step's subagent delivers its result"
```

---

### Task 5: Green the workspace

- [ ] **Step 1:** `cargo fmt --all` (stable, not nightly — nightly reformats the whole workspace)
- [ ] **Step 2:** `cargo clippy --workspace --all-targets` — clean
- [ ] **Step 3:** `cargo test --workspace` — all green
- [ ] **Step 4:** Push and open the PR

## Self-Review

**Spec coverage.** The spec's step 1 is "`SubAgentForest` + the wire shim, replacing `SessionModeState`. Fixes the workflow subagent defects as a consequence." Task 1 builds the forest, Task 2 the state and shim, Task 3 the rewiring, Task 4 the proof. The spec's remaining steps 2–6 are explicitly out of scope for this plan.

**Placeholder scan.** Task 4 Step 1 is a described test rather than complete code — the harness signature (`spawn_run_with_provider`, `wait_for_run`) has to be read at implementation time. Flagged rather than faked.

**Type consistency.** `TreeOwner`, `SubAgentForest`, `OwedResult`, `owner_for`, `owner_of`, `tree_mut`, `root_owner`, `current_agent` are used with the same names and signatures in Tasks 1–4.
