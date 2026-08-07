# The session actor as composed components

## Context

`session_actor.rs` reached 7,327 lines before PR #259 split it into an actor, a context provider, a hook layer and a test file. That split was a pure move; it made the code legible without changing how features land. This design addresses the reason it grew.

Since #101 — the last time the file shrank, at 1,521 lines — it gained 5,806 lines:

| change | lines | kind of change |
|---|---|---|
| subagents (#116) | +1,234 | capability |
| hooks (#211/#215/#216) | +976 | capability |
| workflows (#184) | +907 | **a new session kind** |
| provisioning (#235/#240) | +581 | capability |
| slash commands (#220) | +467 | capability |
| plugin agents (#219) | +393 | capability |
| asks, reads (#119/#149) | ~+650 | capability |

Workflows — the only new *kind* — accounts for about 16%. The other 84% is capabilities that cut across every kind: subagents work in a conversation and in a workflow step; hooks fire for main agents, subagents and steps alike.

Every one of those changes added variants to the same command enum, the same event enum, and arms to the same two matches. Nothing was ever taken back out. The actor today owns five unrelated protocols and demultiplexes between them at runtime.

### The defect this shape already caused

`SessionModeState` owns the subagent tree:

```rust
pub enum SessionModeState {
    Interactive { subagents: SubAgentTree },
    Workflow(WorkflowRunState),          // trees hang off run.steps[i].subagents
}
```

So it needs two accessors. `tree_of_parent_mut` / `tree_of_node_mut` have correct `Workflow` arms and are used by every **write**. `subagents()` returns `empty_tree()` for a run and is used by every **read** — thirteen call sites. `trees()`, written to span a run's per-step trees, has **zero callers**.

The consequences in a workflow run, all from reading the code rather than executing it:

| read site | consequence |
|---|---|
| `on_sub_agent_outcome` | lookup fails, outcome dropped with a warning; result never delivered or journaled |
| `PrepareOffload` (`has_active`) | false, so the session can unload while a step's subagent is mid-run |
| `ReconcileSubAgents`, `on_recovery_complete` | interrupted nodes in a run are never reconciled |
| `SpawnSubAgent` (`active_count`) | 0, so the concurrency cap is unenforced |
| `SubAgentStatus`, `SubAgentTree` | the tool and the API report nothing |
| `perform`, `resolve_agent` | a cold node cannot be respawned |

Workflow support is not finished, so these are accepted for now. They are recorded here because they are not a workflow bug — they are what happens when a capability is written against one kind's shape and the other kind silently inherits a no-op. Preventing that class of defect is the point of this design.

**One gap the forest does not close.** Owed results reach a parent through `wake_owed_parents`, which handles parents that are *subagents*. A conversation's main agent is served instead by `main_turn`, which merges owed results into its next turn. A workflow step has no equivalent: nothing resumes a step with its own subagent's result, because `perform` deliberately refuses to resume an `AgentKey::Step` — steps are started by `perform_run_action` and nothing else. So after this change a step's *nested* subagents work (sub wakes sub), and a step's *direct* subagent is recorded, reported and counted but not delivered back to the step. Closing that is a separate piece of work: it means teaching the run driver to resume a step in flight, which is a change to how a step's turn is defined rather than to where the tree lives.

## Goals

- A new capability lands as a new module plus roughly a dozen lines of shared wiring, instead of ~1,200 lines threaded through shared matches.
- A capability cannot serve one session kind and no-op in another, because it does not know kinds exist.
- Subagents work inside workflow steps through the same code path a conversation uses, with no workflow-specific branch in the subagent code.
- No file over ~1,000 lines.

## Non-goals

- **Splitting `SessionActor` into several actor types.** One session is one journal (`session/<id>`), one status the supervisor caches, one offload protocol. Separate actors would duplicate all of it.
- **Restructuring the persisted event enum.** `SessionDomainEvent` stays one flat enum on the wire. Renaming persisted variants is what broke the supervisor in #101.
- **A `Box<dyn Component>` registry.** Components are named fields. See "Why components are fields".
- Fixing the workflow subagent defects as a separate piece of work. They fall out of moving the tree.

## The model

`SessionActor` becomes session context, state and journaling — nothing else. Every concrete behaviour is a component, and all components follow one pattern:

- **handle a command** → return the events it produced
- **apply an event** → fold it into state
- **decide from state** → say what should start now

The actor delegates commands and owns persistence. It makes no decisions of its own.

### The one framework constraint that shapes this

```rust
fn apply_event(state: Self::State, event: Self::Event) -> Self::State;   // no &self
```

`horsie_actor` folds with no instance in scope, which is what guarantees replay is reproducible from the journal alone. So the fold half of a component must be reachable from the type, not through a trait object.

### Why components are fields, not a registry

Because of the above, `Box<dyn Component>` cannot dispatch the fold. Components are therefore named fields on the actor, and dispatch is a `match` rather than a loop. This is simpler than a registry and there is no case for the extra machinery: there are six components, all known at compile time.

## The `Component` trait

```rust
/// One concern of a session: a slice of state, the commands that change it, and
/// the events that record the change.
///
/// A component never mutates. `handle` decides and returns events; the actor
/// persists them and folds them through `apply`. That is the same discipline
/// `EventSourcedActor` imposes on the actor as a whole, applied one level down,
/// and it is what makes a crash mid-command safe: nothing happened unless it was
/// journaled.
///
/// A component may **read** any part of `SessionState` — `Turns` reads the
/// subagent forest to know which finished results ride its next turn. It may
/// **write** only its own slice, through its own events. Reading across is what
/// keeps components from needing to talk to each other; writing across is what
/// this trait exists to prevent.
///
/// Crucially, a component never learns which *kind* of session it is in. There
/// is no `if workflow` anywhere below this trait. A component that needs to know
/// which agent roots a tree asks the state, not the session.
#[async_trait]
pub trait Component: Send + Sync {
    /// The commands this component owns. Nested under one `SessionCommand`
    /// variant, so the actor dispatches by variant and no component is ever
    /// offered a command that is not its own.
    type Command: Send + 'static;

    /// Handle one command: do whatever side effects it needs — spawn a child,
    /// message an agent, answer a caller — and return what should be persisted.
    ///
    /// Side effects happen here and state changes do not. A command that
    /// decides nothing returns `CommandEffect::none()`; a command that must
    /// answer a caller does so on the reply channel it was given, whether or not
    /// it persists anything.
    async fn handle(
        &mut self,
        cx: &mut SessionCx<'_>,
        state: &SessionState,
        cmd: Self::Command,
    ) -> CommandEffect<SessionDomainEvent>;

    /// Fold one of this component's events into its slice of state.
    ///
    /// An associated function rather than a method: replay runs with no instance
    /// in scope, which is precisely what guarantees a recovered session and a
    /// live one follow the same path. Must be pure — no I/O, no clock, no
    /// randomness. Anything read here must come from the event or from state,
    /// never from the component's own fields.
    fn apply(state: &mut SessionState, event: &SessionDomainEvent);

    /// What this component wants started, given the state as it now is.
    ///
    /// This is the other half of `handle`, and the reason the session gets
    /// anything done at all: most work here is not triggered by a command.
    /// A workflow advances because the previous step concluded. A finished
    /// subagent's result gets delivered because it finished. Nobody asked for
    /// either — an event landed, the state changed, and something became
    /// startable.
    ///
    /// Called at every turn boundary, on every component, and the results are
    /// concatenated. A component returns only work **it** owns: `Turns` returns
    /// a main-agent turn, `SubAgents` returns a wake for an idle parent owed its
    /// children's results, `WorkflowRun` returns the next step. Nothing here
    /// crosses components, which is why the boundary is a concatenation and not
    /// a negotiation.
    ///
    /// Pure, like `apply` — no actors, no I/O, no clock. The actor performs what
    /// this returns; deciding and performing stay apart so the decision is
    /// testable against a hand-built state.
    ///
    /// A component with nothing to start — `Reads`, `HookRouting` — takes the
    /// default and never thinks about boundaries.
    fn actions(&self, _state: &SessionState) -> Vec<AgentAction> {
        Vec::new()
    }

    /// One command this component wants to send itself once recovery finishes,
    /// to repair whatever a dead process left behind.
    ///
    /// A self-send rather than direct work: recovery must not persist, and this
    /// runs before the first live command, so anything that needs to journal has
    /// to arrive as an ordinary command. `SubAgents` sends itself a reconcile
    /// when it finds nodes still `Running`; `RuntimeLifecycle` re-attempts a
    /// create the process died inside.
    fn on_load(&self, _state: &SessionState) -> Option<SessionCommand> {
        None
    }

    /// Whether this component has work in flight, so the supervisor must not
    /// unload the session.
    ///
    /// Asked of every component and OR-ed. This is the invariant that keeps a
    /// forty-minute tool call from being unloaded out from under itself, and it
    /// has to be per-component: today it is one hand-written condition that
    /// checks the main agent's status and one subagent accessor, and a
    /// capability added later has no way to make itself heard.
    fn busy(&self, _state: &SessionState) -> bool {
        false
    }
}
```

### What a component is handed

```rust
/// Everything a component needs to act, and nothing it could use to cheat.
///
/// No `&mut SessionState`: state changes only by folding events. No journal
/// handle: persistence is the actor's. What is here is the identity a component
/// needs to address the outside world.
pub struct SessionCx<'a> {
    /// id, spec, deps, supervisor link, positions — immutable for the actor's life.
    pub owned: &'a SessionOwned,
    /// `AgentKey -> ActorRef<AgentCommand>` for this session's live agents.
    /// Mutable: spawning a subagent registers one.
    pub roster: &'a mut AgentRoster,
    /// For spawning child actors.
    pub actor: &'a ActorContext<SessionActor>,
}
```

## State

The subagent forest moves out of `SessionModeState`, which disappears. `SessionState` becomes flat, one slice per component:

```rust
pub struct SessionState {
    /// Written by RuntimeLifecycle, Turns and WorkflowRun alike — a session is
    /// provisioning, or running a turn, or running a step, and those are
    /// mutually exclusive by nature. The fold is sequential over one ordered
    /// stream, so the last event wins, which is the right answer.
    pub status: SessionStatus,
    pub last_error: Option<String>,
    /// Every agent's banked usage. Core-owned: all three agent-owning
    /// components record into it.
    pub agent_usage: HashMap<String, UsageTotal>,

    // Turns
    pub inbox: Vec<InboxMessage>,
    pub pending_asks: Vec<PendingAsk>,

    // WorkflowRun
    pub run: Option<WorkflowRunState>,

    // SubAgents
    pub subagents: SubAgentForest,
}
```

### `SubAgentForest`

```rust
/// Which agent roots a subagent tree. A conversation has exactly one; a
/// workflow run has one per step execution.
pub enum TreeOwner { Main, Step(Uuid) }

/// Every subagent this session holds, whatever kind of session it is.
///
/// Keyed by owner rather than nested inside the session's mode, which is the
/// whole fix: there is no accessor that can see one kind's subagents and miss
/// another's, so the aggregate queries below are correct for a workflow run the
/// day they are written.
pub struct SubAgentForest {
    trees: BTreeMap<TreeOwner, SubAgentTree>,
}

impl SubAgentForest {
    // Per-tree, for the agent doing the spawning.
    pub fn tree(&self, owner: TreeOwner) -> Option<&SubAgentTree>;
    pub fn tree_mut(&mut self, owner: TreeOwner) -> &mut SubAgentTree;  // created on first spawn
    pub fn owner_of(&self, node: Uuid) -> Option<TreeOwner>;

    // Per-node, owner-agnostic: the caller no longer has to know the kind.
    pub fn node(&self, id: Uuid) -> Option<&SubAgentRecord>;
    pub fn depth_of(&self, owner: TreeOwner, parent: SubAgentParent) -> Option<u32>;
    pub fn visible_to(&self, caller: SubAgentParent, id: Uuid) -> bool;

    // Whole-forest aggregates. These are the five that are wrong today.
    pub fn active_count(&self) -> u32;
    pub fn has_active(&self) -> bool;
    pub fn interrupted(&self) -> Vec<Uuid>;
    pub fn owed(&self) -> Vec<OwedResult>;
}

/// One finished subagent's result that its parent has not been sent.
pub struct OwedResult {
    pub child: Uuid,
    pub parent: SubAgentParent,
    pub owner: TreeOwner,
    pub part: SubAgentResultPart,
}
```

`SessionState` is snapshotted, so this is a durability change. `mode.rs` already carries a `SessionModeWire` shim for exactly this kind of move; the same technique applies, with a round-trip test over a pre-move snapshot. This is the #101 risk area and gets its own task.

## Commands

Commands are never persisted, so nesting is free and carries no migration:

```rust
pub enum SessionCommand {
    Lifecycle(LifecycleCommand),   // Provision, FinishProvisioning, PrepareOffload, Delete
    Turn(TurnCommand),             // UserMessage, Stop, Answer, ReconcileInterrupted
    Run(RunCommand),               // AdvanceRun, RetryStep, RunState
    SubAgent(SubAgentCommand),     // Spawn, FinishSpawn, Status, Tree, Reconcile
    Read(ReadCommand),             // ReadLog, PageLog, AgentState, Snapshot, UsageStats
    Hooks(HookCommand),            // HooksRan, HaltAgent, ContinueAfterStop
    Core(CoreCommand),             // SetSessionTitle, Progress
    AgentOutcome(AgentOutcome),
}
```

## Events

`SessionDomainEvent` keeps its current flat shape — this is the persisted contract. It gains a classifier so the fold can dispatch:

```rust
pub enum EventDomain { Core, Lifecycle, Turn, Run, SubAgent }

impl SessionDomainEvent {
    /// Which component's fold owns this event. One arm per variant, listed
    /// rather than defaulted: a newly added event must be classified on purpose.
    pub fn domain(&self) -> EventDomain;
}
```

| domain | events |
|---|---|
| Core | UsageRecorded |
| Lifecycle | ProvisioningStarted, ProvisioningSucceeded, ProvisioningFailed |
| Turn | MessageQueued, TurnBegan, AskRecorded, TurnEnded, TurnFailed, TurnStopped, TurnInterrupted, SessionFailed |
| Run | StepStarted, StepConcluded, StepFailed, StepCancelled, RunFinished, RunFailed |
| SubAgent | SubAgentSpawned, SubAgentRunning, SubAgentCompleted, SubAgentFailed, SubAgentNotified |

## The components

### `SessionCore`

A component like the rest, but the one whose slice is the session's own bookkeeping rather than a feature. Owns `UsageRecorded`, because all three agent-owning components record into `agent_usage`; handles the title and progress commands, which only talk to the supervisor and to an agent's log.

The `AgentOutcome` demux is not its job — that is routing, and it lives in the actor's dispatch beside the variant match. Worth noting because it gets substantially simpler: today it needs `state.mode.run()`, `run.index_of_agent()` and a comparison against `self.id`; with a roster keyed by `AgentKey` it is one lookup.

### `RuntimeLifecycle`

Gets this session a sandbox and knows whether it has one. Journals its intent *before* calling the vendor, so an interrupted create is discoverable at load; runs the create off the mailbox so reads and stops stay answerable throughout; re-attempts at load if it finds one unfinished.

Exposes one thing to everyone else — `ready(state)`, the single gate the boundary checks before asking any component what to start. That predicate is the whole of provisioning's coupling to the rest of the session.

*Commands:* Provision, FinishProvisioning, PrepareOffload, Delete
*Events:* the three Provisioning ones · *State:* contributes `status`

### `Turns`

The conversation, and the only component a person drives directly. Makes a user message durable before doing anything with it, so an accepted message survives a crash and is still owed an answer. Merges the queue into one turn at the next boundary. Parks on questions and refuses a partial answer set, because a half-answered park leaves the wire holding a `tool_use` with no result. Records how each turn ended.

Returns no actions when `state.run` is set, and rejects a user message there: a run works from its definition and there is nobody to send a message to.

*Commands:* UserMessage, Stop, Answer, ReconcileInterrupted
*Events:* MessageQueued, TurnBegan, AskRecorded, TurnEnded/Failed/Stopped/Interrupted, SessionFailed
*State:* `inbox`, `pending_asks`, `status`

### `WorkflowRun`

The graph. Reads the run log, evaluates the transition out of the last concluded step, and decides the next step, the run's end, or its failure. Appends rather than replaces, so a loop back onto a step and a retry of one are both new entries and the projection stays lossless.

Returns no actions when `state.run` is `None`. That check, and not a construction-time branch, is what makes this component inert in a conversation.

*Commands:* AdvanceRun, RetryStep, RunState
*Events:* StepStarted/Concluded/Failed/Cancelled, RunFinished/Failed · *State:* `run`, `status`

### `SubAgents`

The tree of delegated work. Enforces depth and concurrency. Persists a spawn *before* the child actor exists, so a crash between the two replays as a node recovery can fail rather than as an untracked agent. Records terminal results, wakes idle parents owed their children's output, reconciles nodes a dead process left running.

Never asks what kind of session it is in. Every query spans the whole forest, which is what makes a workflow step's subagents work through the identical code path a conversation's use.

*Commands:* Spawn, FinishSpawn, Status, Tree, Reconcile
*Events:* the five SubAgent ones · *State:* `subagents`

### `Reads`

Answers questions without waking anything. Every read is served from the resident actor's memory or forwarded to the agent that owns the transcript; none touches the journal, so opening a session to look at it costs no sandbox.

*Commands:* ReadLog, PageLog, AgentState, Snapshot, UsageStats · *Events:* none · *State:* none

### `HookRouting`

Pure routing for what plugins did. Forwards records into the agent's own transcript, resumes an agent a `Stop` hook held open, and turns a hook's halt into an ordinary failure — so what a failure *means* stays decided in one place rather than branching per agent kind.

*Commands:* HooksRan, HaltAgent, ContinueAfterStop · *Events:* none · *State:* none

`Reads` and `HookRouting` implement one method each and take every default. That is the pattern paying off: a component costs only what it uses.

## The actor

```rust
pub struct SessionActor {
    /// Identity and dependencies: id, spec, deps, supervisor link, positions.
    /// Immutable for the actor's life, so it can be lent out while the
    /// components below are borrowed mutably.
    owned: SessionOwned,
    /// `AgentKey -> ActorRef<AgentCommand>`. Separate from `owned` because
    /// components mutate it (spawning a subagent registers one).
    roster: AgentRoster,

    core: SessionCore,
    lifecycle: RuntimeLifecycle,
    turns: Turns,
    run: WorkflowRun,
    subagents: SubAgents,
    reads: Reads,
    hooks: HookRouting,
}
```

`SessionCore` is a component like the rest — it implements the same trait — but it is the one whose slice is the session's own bookkeeping rather than a feature. `owned` and `roster` sit beside it rather than inside it so that lending a `SessionCx` does not conflict with borrowing a component mutably.

Four wiring points, each a handful of lines.

```rust
// 1. Delegate. Was a 449-line match.
//
// `cx` borrows the core's addressing half (roster, supervisor, spec) while the
// component fields are borrowed mutably alongside it — so the core's own
// owned-state and its cx-lending half are separate fields, not one struct.
// Getting that borrow split right is an implementation task, not a design one.
async fn handle_command(&mut self, state, cmd, ctx) -> CommandEffect<SessionDomainEvent> {
    let mut cx = SessionCx::new(&self.owned, &mut self.roster, ctx);
    match cmd {
        SessionCommand::Lifecycle(c) => self.lifecycle.handle(&mut cx, state, c).await,
        SessionCommand::Turn(c)      => self.turns.handle(&mut cx, state, c).await,
        SessionCommand::Run(c)       => self.run.handle(&mut cx, state, c).await,
        SessionCommand::SubAgent(c)  => self.subagents.handle(&mut cx, state, c).await,
        SessionCommand::Read(c)      => self.reads.handle(&mut cx, state, c).await,
        SessionCommand::Hooks(c)     => self.hooks.handle(&mut cx, state, c).await,
        SessionCommand::Core(c)      => self.core.handle(&mut cx, state, c).await,

        // The demux. Resolve the agent, then hand the outcome to whichever
        // component owns that agent — the one place a command is routed by
        // identity rather than by variant.
        SessionCommand::AgentOutcome(o) => match self.roster.key_of(o.agent_id()) {
            Some(AgentKey::Main)     => self.turns.on_outcome(&mut cx, state, o).await,
            Some(AgentKey::Sub(id))  => self.subagents.on_outcome(&mut cx, state, id, o).await,
            Some(AgentKey::Step(id)) => self.run.on_outcome(&mut cx, state, id, o).await,
            None => CommandEffect::none(),   // an agent that has already gone
        },
    }
}

// 2. Fold. Static dispatch, one arm per component.
fn apply_event(mut state: SessionState, event: SessionDomainEvent) -> SessionState {
    match event.domain() {
        EventDomain::Core      => SessionCore::apply(&mut state, &event),
        EventDomain::Lifecycle => RuntimeLifecycle::apply(&mut state, &event),
        EventDomain::Turn      => Turns::apply(&mut state, &event),
        EventDomain::Run       => WorkflowRun::apply(&mut state, &event),
        EventDomain::SubAgent  => SubAgents::apply(&mut state, &event),
    }
    state
}

// 3. The turn boundary. Every component contributes; concatenation, not negotiation.
fn next_actions(&self, state: &SessionState) -> Vec<AgentAction> {
    if !self.lifecycle.ready(state) {
        return Vec::new();      // nothing starts before the runtime it would run on exists
    }
    [
        self.subagents.actions(state),
        self.turns.actions(state),
        self.run.actions(state),
    ]
    .concat()
}

// 4. Recovery. Each component repairs itself.
async fn on_recovery_complete(&mut self, state, ctx) {
    self.core.restore_agents(state, ctx);
    for cmd in [
        self.lifecycle.on_load(state),
        self.turns.on_load(state),
        self.run.on_load(state),
        self.subagents.on_load(state),
    ]
    .into_iter()
    .flatten()
    {
        let _ = ctx.self_ref().tell(cmd).await;
    }
}
```

Offload becomes a per-component question rather than one hand-written condition:

```rust
fn can_offload(&self, state: &SessionState) -> bool {
    ![
        self.turns.busy(state),
        self.run.busy(state),
        self.subagents.busy(state),
    ]
    .contains(&true)
}
```

## What adding a capability costs

One new module, plus: one field on `SessionActor`, one `SessionCommand` variant, its events plus their `domain()` arms, and one line each in `next_actions`, `on_recovery_complete` and `can_offload`. Roughly a dozen lines of shared code.

Subagents cost ~1,234 lines threaded through three shared matches. That is the delta this design buys.

## Testing

- **Component decisions are unit tests.** `actions`, `apply` and `busy` are pure functions of a hand-built `SessionState`, so each component's decisions test without an actor, a runtime or a journal. This is already true of `orchestrator.rs` and is the model.
- **Snapshot round-trip.** A snapshot written before the forest move must load with every subagent intact. This is the #101 risk and needs an explicit test with a captured pre-move payload, not a synthesised one.
- **`domain()` totality.** Every `SessionDomainEvent` variant classifies; the match is exhaustive so a new event fails to compile until classified.
- **The workflow subagent path.** A test that a subagent spawned by a workflow step delivers its result to that step — the defect this design exists to make unrepresentable. It should fail against `main` today.
- **Existing coverage carries over.** The 569 server tests are the regression suite; a pure-behaviour refactor keeps them green.

## Sequencing

Each step ships green on its own.

1. `SubAgentForest` + the wire shim, replacing `SessionModeState`. Fixes the workflow subagent defects as a consequence. Biggest durability risk; goes first and alone.
2. Nest `SessionCommand`; collapse `handle_command` to a delegating match.
3. Add `EventDomain` + `domain()`; split `apply_event` into per-component folds.
4. Extract `Reads` and `HookRouting` — no state, no events, lowest risk.
5. Extract `RuntimeLifecycle`, then `SubAgents`, then `Turns` and `WorkflowRun`.
6. Introduce the `Component` trait once two components exist and its shape is settled by use rather than guessed.

## Judgment calls (vetoable)

- **No `Box<dyn Component>` registry.** Six components, all known at compile time, and the framework's `apply_event` has no `&self` so the fold cannot dispatch dynamically anyway.
- **The `Component` trait is written last, not first.** Writing it before there are two implementors would be guessing at the interface. Steps 4–5 establish the shape; step 6 names it.
- **Workflow is not a "kind".** `Turns` and `WorkflowRun` are ordinary components that go quiet based on `state.run`, which deletes `SessionModeState` and the construction-time branch entirely.
- **A routine is not a third kind.** It is a conversation with `unattended = true`, which is already how the code works.
- **`status` stays a single shared field** written by three folds. Deriving it would be a larger change for no current benefit.
