# Session Runners and Capabilities Design

**Date:** 2026-08-15
**Status:** Draft — awaiting review

## Context

A session today hosts four kinds of agent — main, fork, subagent, workflow step — and the differences between them are spread across the session actor rather than held anywhere. `AgentKey` has four variants; `SessionAgentKind` has four more; and the pair is matched in roughly eight places: the toolbox layers and prompt suffix in `context.rs`, `effective_settings`, `stop_boundary`, `reach`, `resolve_agent`, the agent-entry projection in `reads.rs`, and the halt handler in `hooks.rs`.

Three of those — `resolve_agent`, `stop_target`, `reach` — answer "what kind of agent is this uuid?" by probing three separate registries in a fixed order: the workflow run log, then the fork roster, then the subagent forest. The order is load-bearing and the code says so; `session_actor/mod.rs` carries a comment recording that answering `Sub` before checking forks made a fork of a fork read as a fork of a subagent.

The immediate driver is a new capability: **an agent should be able to invoke a workflow**, any agent, any number of times, and eventually with a graph built at runtime rather than looked up by name. That does not fit the current shape. `SessionState.run` is a single `Option<WorkflowRunState>`, the graph lives on the `SessionSpec` frozen at session creation, and a subagent's owning tree is inferred from "which step is in flight" (`root_owner()`) — an inference with no answer once two runs are live.

Rather than widen the existing shape, this redesign replaces the organising idea. A session becomes a host for **runners**; a runner owns a unit of work and the agents that carry it out; and everything an agent is equipped with is a **capability** held by its runner.

## Goals

- Any agent may create any number of subagents and workflow runs, nested arbitrarily.
- A workflow's graph is journal data, so an ad-hoc graph becomes possible without further structural change.
- A workflow session — a run with no conversation — keeps working, as a session whose root runner is a workflow.
- Deleting the per-kind matches: one dispatch by owner, not by kind.
- Session state carries structure and aggregates only; nothing that belongs to one agent.

## Non-goals

- The `invoke_workflow` tool itself, and ad-hoc graph construction. This spec makes both expressible; neither ships here.
- Multiple runtimes per session. The runtime becomes a runner, which makes this possible later; one is still the rule.
- Any change to `AgentActor`, the agent journal, or the provider layer.

## Locked decisions

1. **One session, many runners.** An invoked workflow runs inside the session that invoked it. There is no child session.
2. **Concurrent runs share the session's workspace.** Runs proceed in parallel, each still running one step at a time. This is the contract subagents already have — one workspace, several writers.
3. **The journal shape breaks.** State fields are renamed and merged and event variants are replaced. Existing sessions are truncated, not migrated.
4. **Runner state is session state.** There is one journal. A runner's slice, and its capabilities' slices inside it, are nested in the session's fold.
5. **Runners decide; the session performs.** A runner returns events and requested actions. It never reaches the registry, never spawns an actor, never writes state.

## Session state

```rust
struct SessionState {
    spec: Option<SessionSpec>,
    /// The conversation or the run this session *is*. Its status is the
    /// session's status.
    root: RunnerId,
    /// Which runner owns each agent. Structure, not content — what an agent
    /// said lives in the agent's own journal.
    agents: BTreeMap<AgentId, RunnerId>,
    /// Tokens by model across everything this session has run. Aggregate
    /// only; the per-agent breakdown belongs to the runner that owns it.
    usage: BTreeMap<ModelAlias, UsageTotal>,
    /// Every runner, and — through `parent` — the shape of the tree they
    /// form. The only place nesting is recorded.
    runners: BTreeMap<RunnerId, RunnerRecord>,
}

struct RunnerRecord {
    kind: RunnerKind,
    /// The agent that created me. `None` for the root and for the runtime.
    /// Provenance, not debt — whether I report is decided by my kind.
    parent: Option<AgentId>,
    /// The same six words for every kind.
    status: RunnerStatus, // Pending Running AwaitingInput Done Failed Cancelled
    /// My slice. The session never looks inside; it hands it back to the
    /// runner that owns it.
    state: RunnerState,
    created_at_ms: u64,
    ended_at_ms: u64,
}
```

Three consequences.

`SessionStatus` stops being journaled and becomes `runners[root].status`, so it cannot drift from what a runner recorded, and a background subagent no longer risks making the session read `Running`.

`agent_usage: HashMap<String, UsageTotal>` leaves session state — it is per-agent detail — and the aggregate is keyed by model instead. That needs the model name on the usage event, where today the map is keyed by agent id and summed.

`SessionKind` shrinks to one job: deciding which runner is the root. `SessionKind::Agent` roots a `ConversationRunner`; `SessionKind::Workflow` roots a `WorkflowRunner` carrying the graph snapshot.

## Runners

Four kinds. A runner impl is behaviour and holds no fields; everything it knows arrives in the state handed to it.

**ConversationRunner** — the session's conversation, and its forks. One struct for both: a fork is a conversation with a branch point.

```rust
struct ConversationState {
    agent: AgentId,
    /// `None` is the session's own; `Some` is a fork and names where it
    /// branched from.
    seed: Option<Branch>,
    turn: TurnStatus,
    title: Option<String>,
    usage: UsageTotal,
    capabilities: Vec<Capability>,
}
```

Its `outcome()` is always `None` — a conversation owes nobody a result, root or not. This is what lets `parent` mean provenance rather than debt, and it collapses `ForkCreated`/`ForkSeeded`/`ForkTitled`/`ForkStatusChanged`/`ForkTurnEnded` into the conversation's own vocabulary. Those five exist today only because a fork moves its roster entry while the main agent moves the session's status; once every runner carries its own status, the distinction is gone.

**SubAgentRunner** — one delegated worker. Reports once, to the agent that asked.

```rust
struct SubAgentState {
    agent: AgentId,
    label: String,
    task: String,
    agent_type: Option<String>,
    usage: UsageTotal,
    capabilities: Vec<Capability>,
}
```

`SubAgentForest`, `SubAgentTree`, `TreeOwner`, `owner_for` and `root_owner` all delete. Depth is a walk up `parent`; a caller's subtree is a walk down.

**WorkflowRunner** — one run of a graph, owning step agents over time.

```rust
struct WorkflowState {
    /// Snapshotted when the run was created — on the runner, not on the
    /// SessionSpec. This is the single change that makes an ad-hoc graph
    /// possible: the graph is journal data, so nothing requires it to have
    /// a name.
    graph: Arc<WorkflowRunSpec>,
    steps: Vec<StepRun>,     // unchanged
    output: Option<Value>,
    error: Option<String>,
    usage: UsageTotal,
    capabilities: Vec<Capability>,
}
```

`WorkflowRunState` and `WorkflowOrchestrator` move here nearly untouched. There is deliberately no `StepRunner` — steps are agents *of* this runner. Give each step its own runner and the graph state has nowhere to live.

A session-initiated run and an agent-invoked run are the same struct. They differ in `parent`: `None` means the session is this run, `Some(agent)` means an agent invoked it. That field drives exactly two things — who receives the terminal output, and whether this runner's status is the session's.

**RuntimeRunner** — the sandbox. The only runner that owns no agents.

```rust
struct RuntimeState {
    provisioned_at_ms: Option<u64>,
    phase: Provisioning | Ready | Failed { terminal: bool } | Released,
    detail: Option<String>,   // the vendor's own words, shown to the user
}
```

It owns the *lifecycle* — provision, narrate, fail, hibernate, release — and deletes `LifecycleCommand` and four event variants from the session's vocabulary. It does **not** own acquisition: an agent's `provide()` reaches the `RuntimeManager` directly, on the agent's own task, which is what keeps a thirty-second toolbox build off the session mailbox. Routing acquisition through the mailbox would reintroduce exactly what that separation was built to avoid, on a per-turn path.

## Capabilities

**Everything an agent is equipped with is a capability.** Not only the things that create child runners — the runtime toolbox, the memory layer, the MCP layer, the control-plane layer, `ask_user`, `set_session_title`, `submit_result` are all equipment, and all arrive the same way.

The set: `Runtime`, `Memory`, `Mcp`, `ControlPlane`, `AskUser`, `Title`, `SubAgents`, `Workflows`, `Forks`, `StepResult`.

```rust
trait Capability {
    /// Equip the agent: toolbox layer, prompt section.
    fn setup(&self, spec: &mut AgentSpec);

    /// `None` means "not mine" — the runner offers it to the next capability.
    /// `Some` means I took it, and here is what to journal and what to do.
    ///
    /// One method rather than a `supports` predicate beside a handler: a
    /// capability that answered yes and then could not cope, or a pair edited
    /// out of step, are states that cannot be written this way.
    ///
    /// `&Message` rather than by value, because the same message is offered
    /// to each capability until one takes it; the taker clones what it keeps.
    fn handle(&self, from: AgentId, msg: &Message)
        -> Option<(Vec<CapEvent>, Vec<Action>)>;

    /// Fold my own slice.
    fn apply(&mut self, e: &CapEvent);
}
```

**Tool calls are offered around; structural messages are looked up.** A tool call goes through `capabilities.iter().find_map(|c| c.handle(from, &msg))`, which is what lets `Runtime` answer for a namespace nobody can enumerate — the sandbox toolbox plus whatever the plugin library scan discovered — while the fixed-name capabilities answer for theirs. A child's outcome and an arriving answer do **not** go through that scan: they route to the capability that created that child or recorded that ask, which is one owner by construction, recorded in that capability's own slice. Scanning there would let two capabilities plausibly claim the same `ChildOutcome`, which is the ambiguity most worth designing out.

Order is therefore the conflict resolution for tool calls, and it must be a written property of assembly rather than an accident of construction: the open-namespace capabilities — `Runtime`, `Mcp` — sort last. This is the behaviour today, where `AskUserToolbox` wraps the plugin toolbox and silently shadows a plugin tool of the same name, so it is not a regression; but it is worth a debug-only assembly pass that offers a synthetic call to every capability and warns when more than one answers.

A capability is a **value**, not a trait impl on the runner — a runner holds a `Vec<Capability>` where `Capability` is a closed enum with one arm per implementation, so the list serializes into the runner's slice and the trait above is implemented for the enum by delegation. There is one implementation of each; per-runner variation is expressed at construction:

```rust
struct SubAgents {
    /// Fixed when the owning runner built this: what children inherit.
    child_settings: AgentSettings,
    /// Which child, and which of my agents asked for it. This one map says
    /// both "is a report still owed" and "who to deliver it to".
    outstanding: BTreeMap<RunnerId, AgentId>,
}

enum SubAgentsEvent {
    Started  { child: RunnerId, from: AgentId },
    Reported { child: RunnerId },
}
```

One implementation is sound because of the invariant in the next section: an agent cannot conclude while it has outstanding children, so every parent does the identical thing on a report — deliver to the agent that asked.

Capability state lives **inside** the runner's state, so it is journaled, recovered, and cannot drift:

```
SessionState
└── runners[R]
     └── state: RunnerState::Workflow(WorkflowState)
          ├── graph, steps, output, usage
          └── capabilities
               ├── SubAgents  { child_settings, outstanding }
               ├── AskUser    { … }
               └── StepResult { … }
```

`CapEvent` is a closed enum with one arm per capability rather than an opaque blob: it keeps the journal typed, and a missing arm is a compile error in the right place.

**Instances belong to the runner; equipment is computed per agent.** A `WorkflowRunner` holds one `SubAgents` and one `AskUser`, because their state outlives any single step. Which capabilities a *given* agent is equipped with is decided at spawn, by folding a subset over its `AgentSpec`. That is how a workflow whose step 1 is interactive and step 2 is not gets exactly the right tools, with one mechanism rather than two — and it replaces the four-arm toolbox match and the four-arm prompt-suffix match in `context.rs`.

A message every capability declined is an error, never a silent drop — `None` from all of them, checked in the one place the scan lives. That check replaces an exhaustive-match compile error the current code relies on, so it is a real downgrade in safety; making it loud is the least that compensates.

## The agent lifecycle

Every runner that owns agents implements one handler; `RuntimeRunner` does not implement it at all, so "a runner with no agents cannot be handed an agent event" is a type fact rather than an unreachable arm.

```rust
/// Every method returns the same pair every decision in this design returns:
/// events to journal, and actions for the session to perform.
type Emit = (Vec<RunnerEvent>, Vec<Action>);

trait AgentLifecycle {
    fn on_agent_started(&self, s: &State, agent: AgentId) -> Emit;
    fn on_agent_ended(&self, s: &State, agent: AgentId, end: TurnEnd) -> Emit;
    fn on_agent_halted(&self, s: &State, agent: AgentId, reason: String) -> Emit;
}
```

`on_agent_ended` needs no switch on *which* agent, because **one runner owns exactly one agent role**. Conversation owns a conversation agent; SubAgent owns a worker; Workflow owns step agents — several over time, all step agents. A runner's state, its agent role and its outcome vocabulary are one triple, and that is the justification for cutting runners this way.

**The child translates.** A `SubAgentRunner` turns `AgentOutcome::{Concluded, Failed, Asked, Parked, Interrupted}` into `SubAgentOutcome::{Completed, Failed}`, and only that reaches the parent. The parent never sees a `TurnEnd` and never learns how a subagent is implemented. Today `on_sub_agent_outcome` carries defensive arms for `Asked` and `Parked` annotated *"a subagent has no ask or timer tools, so neither outcome should ever occur"* — defence written because the translation had no home. It has one now.

## Starting work

There is no `run()` or `init()`. A runner answers:

```rust
fn actions(&self, state: &State, view: &SessionView<'_>) -> Vec<Action>;
```

Pure and idempotent, called at every boundary. Creation and recovery then take the same path: `RunnerCreated` folds to `status: Pending` with no agents, and `actions()` says "start the first agent" — whether that state arrived a millisecond ago or from a journal replayed after a restart. A `run()` that fires once would need a second entry path for recovery, which either double-starts every agent or has to be suppressed.

```rust
enum Action {
    StartAgent  { agent: AgentId, spec: AgentSpec, first: Incoming },
    CreateChild { kind: RunnerKind, args: RunnerArgs, parent: AgentId },
    Deliver     { to: AgentId, from: RunnerId, part: ResultPart },
    Cancel      { agent: AgentId },
}
```

One gate in front of all of it: nothing starts unless the `RuntimeRunner` is `Ready`.

## Routing

Every message from an agent goes to the runner that owns it:

```rust
let runner = state.agents[&agent];
```

That single lookup replaces `on_agent_outcome`'s identity probing, `stop_target`'s three-registry walk, and the same walk in `resolve_agent` and `reach`. Within the runner, a tool call is offered to each capability until one takes it, and a structural message is looked up in the capability that owns it.

Usage, turn-preparation progress and hook records mean the same thing for every runner; the session answers those itself rather than routing them, so no runner grows a tail of variants it ignores.

Cross-runner facts are **pushed, never read**: a runner is handed `on_child_result`, `on_agent_ended`, `on_runtime_ready`. It reads its own slice plus a small `SessionView` (is the runtime ready, what is my depth, what is the spec). Handing every runner the whole `SessionState` would let a workflow runner read a conversation's turn status, which is the coupling this removes.

## Ordering

Two orderings, opposite on purpose, and getting either backwards is the easiest mistake in the rewrite.

**Creation persists first.** `RunnerCreated` is durable before the child's agent exists, and the tool's reply fires only after the journal ack — otherwise a crash between spawn and persist hands the model an id for an agent that does not exist. A crash before the ack replays as no child at all, which is strictly better than an untracked agent. The capability therefore never holds the `ReplyTo`; the session owns the deferred reply, as `FinishSpawn` does today.

**Delivery tells first.** The report is enqueued into the parent's agent, and only then is the acknowledgement persisted. A crash in that window replays as a report still owed, and it is delivered again — at-least-once, never lost.

Which makes the delivery flow two batches, not one:

```
worker ends
  session: agents[B] -> R2
  R2.on_agent_ended(B, Concluded)          -> [R2::Concluded { output }]
  PERSIST #1                                                        <- durable
  fold: R2.status = Done
  ─── boundary ───
  scan: R2 is Done, parent = A, and W's SubAgents still lists R2
  R2::outcome() -> SubAgentOutcome::Completed { … }                 <- translation
  W.subagents.handle(ChildOutcome) -> (Reported { R2 }, Deliver { to: A })
  perform Deliver: Enqueue into A                                   <- tell
  PERSIST #2                                                        <- durable
```

One batch would leave no re-drive point. With two, a crash between them replays into "R2 is Done and W still lists it outstanding", so the boundary scan notices again. That is what makes `outstanding` the single fact recording both whether the parent has been told and which agent to tell — there is no separate `notified` flag to disagree with it.

## Recovery

Nothing separate is recovered. There is one journal — the session's. Replay folds every event, including `Runner(R, SubAgents(Started { child, from }))`, through `SessionState::apply_event`, which routes to `runners[R]`, which routes to the capability owning that arm. When the fold ends, every runner and every capability holds exactly what the log says.

The session then instantiates the runner impls from `RunnerRecord.kind` and calls `actions()` on each. That is the whole of recovery.

The only things outside the fold are live actor handles, which are connections rather than state, rebuilt on demand exactly as a cold subagent's actor is spawned on demand today.

## Invariants

1. A runner writes only its own slice. Cross-runner facts arrive as calls.
2. A runner returns events; `apply` is the only writer. Nothing mutates state directly.
3. A runner impl holds no fields. All state is in the session's fold.
4. `actions()` is pure and idempotent.
5. One runner owns exactly one agent role.
6. **An agent may not conclude while it has outstanding children.** Load-bearing: it is what makes a single `SubAgents` implementation correct for all three parent kinds. Needs an enforcement point — `submit_result` refused, or the conclusion deferred, while `outstanding` is non-empty — and a test.
7. A structural message has exactly one owning capability, found by lookup rather than by offering it around. A tool call declined by every capability is an error.
8. Capability order within a runner is a written property of assembly — open-namespace capabilities last — not an accident of construction.

## What this deletes

- `fork.rs` and `forks.rs` — a fork becomes a conversation with a branch point.
- The subagent forest in `subagents.rs` — `SubAgentForest`, `TreeOwner`, `owner_for`, `root_owner`.
- The three-registry probes in `resolve_agent`, `stop_target` and `reach`.
- The four-arm matches in `context.rs` for toolbox layers and prompt suffix, plus `build_memory_layer` and `build_control_layer` as special cases.
- `AgentKey` and `SessionAgentKind` — one flat `AgentId` space, owner resolved by lookup.
- `effective_settings` and `effective_settings_for_parent`.
- The defensive `TurnEnd::Asked`/`Parked` arms in `on_sub_agent_outcome`.

Roughly 2,600 lines net removed, against a rewrite of the most load-bearing actor in the server.

## Open items

**Concurrency cap.** The limit is a property of the sandbox — how many agents may run at once — so it moves to the session and is checked before dispatch, beside "does this agent have this tool". Today's per-caller cap is an artifact: a workflow session has no session-wide `AgentSettings`, so the number had to come from the step's preset. Per-runner sub-budgets can be added later as a value on the runner if fan-out starvation turns out to be real; not now.

**A superseded step.** If a step agent's execution ends before a subagent it spawned finishes, delivery wakes an agent whose step is closed, and a second conclusion lands on an index the run already routed past. Invariant 6 is the intended answer — the step cannot conclude while children run — so this stays open pending that enforcement. Note this looks like a latent defect in the current code too: `owed_deliveries` deliberately routes to the superseded step (`orchestrator.rs`: *"a step that has since been superseded is still what asked"*), and `on_step_outcome` maps the second conclusion onto `StepConcluded { index }`, whose fold overwrites the entry's output and resets the run to `Running`. Worth a test before designing around it.

**Cancel cascade.** Cancelling a runner must cancel the runners parented on its agents, recursively — the same walk a depth budget uses. Not yet designed; retrying a step today would leave an invoked run orphaned with a dead parent agent.

**Recursion budget.** One depth number across the combined runner tree, replacing `MAX_SUBAGENT_DEPTH`. Without it, a workflow whose step invokes the same workflow has nothing bounding it.

**Delivery scan cost.** The boundary scan is over `runners where status == Done && parent.is_some()`, then a lookup in the parent's capability. Same shape as today's `owed()`; fine at tens of runners, worth indexing if a session ever holds hundreds.

## Testing

- Recovery: a journal cut mid-create replays to no child; cut between the two delivery batches replays to a re-delivered report.
- One runner, one agent role: a workflow's step agents all end through the same path.
- Capability dispatch: a tool call every capability declines errors rather than vanishing; a fixed-name capability wins over the open-namespace one that sorts after it.
- Nesting: a subagent of a step agent behaves identically to a subagent of a main agent, and a workflow invoked by a subagent delivers its terminal output to that subagent.
- Invariant 6: an agent that tries to conclude with outstanding children does not.

## Sequence reference

Two diagrams produced during design, kept for reference:

- Who talks to whom — subagent vs main agent vs step agents: <https://excalidraw.com/#json=wywPh4h34-krHI5wp1yT4,shNFa-M-FnxfQ3vSYnhZHA>
- A step agent spawning a subagent, end to end: <https://excalidraw.com/#json=EmuMLsI1ToBQmEfNO5GUO,oeF8Aac09Xwz3yyC4hTuGQ>
