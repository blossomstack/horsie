# Session Runners and Capabilities Design

**Date:** 2026-08-15
**Status:** Revised 2026-08-17 — capabilities moved to the agent actor

## Context

A session today hosts four kinds of agent — main, fork, subagent, workflow step — and the differences between them are spread across the session actor rather than held anywhere. `AgentKey` has four variants; `SessionAgentKind` has four more; and the pair is matched in roughly eight places: the toolbox layers and prompt suffix in `context.rs`, `effective_settings`, `stop_boundary`, `reach`, `resolve_agent`, the agent-entry projection in `reads.rs`, and the halt handler in `hooks.rs`.

Three of those — `resolve_agent`, `stop_target`, `reach` — answer "what kind of agent is this uuid?" by probing three separate registries in a fixed order: the workflow run log, then the fork roster, then the subagent forest. The order is load-bearing and the code says so; `session_actor/mod.rs` carries a comment recording that answering `Sub` before checking forks made a fork of a fork read as a fork of a subagent.

The immediate driver is a new capability: **an agent should be able to invoke a workflow**, any agent, any number of times, and eventually with a graph built at runtime rather than looked up by name. That does not fit the current shape. `SessionState.run` is a single `Option<WorkflowRunState>`, the graph lives on the `SessionSpec` frozen at session creation, and a subagent's owning tree is inferred from "which step is in flight" (`root_owner()`) — an inference with no answer once two runs are live.

Rather than widen the existing shape, this redesign replaces the organising idea. A session becomes a host for **runners**; a runner owns a unit of work and the agents that carry it out; and everything an agent is equipped with is a **capability**, held by the agent itself.

## The design in six sentences

If a piece of the implementation cannot be placed in this paragraph, it is wrong.

> A **session** is one sandbox, a set of **runners**, and the tree they form.
>
> A **runner** is one unit of work — a conversation, a delegated task, a workflow run. It owns the **agents** that carry it out.
>
> An **agent** is one LLM loop and one journal. A **capability** is one thing that agent can do.
>
> When an agent is loaded, its capabilities' `setup` runs in order, each acquiring what it needs and filling in the **agent spec** the loop then runs with.
>
> While it runs, the **agent actor** routes every tool call, answer, child report and turn boundary to its capabilities, folds what they record into its own journal, and performs what they ask for.
>
> The **session actor** is infrastructure: it starts runners, forwards what people send, and tracks the tree. It holds no capabilities and no per-agent state.

Four nouns and two verbs. Capabilities only ever **decide**; the agent actor is the only thing that **acts** — except for `setup`, which is async and runs before the loop starts. There is no fifth kind of object: anything that is not a runner, a capability, an agent or an actor is a mistake, and two drafts of this design had one. The first was a nameless "equipment builder" holding async work a synchronous `setup` could not do. The second was a bundle of four types — `Description`, `Listing`, `AgentDescription`, `RunDescription` — invented so the read side could avoid a per-kind match. Both were caught the same way: by asking which of the four nouns they were, and getting no answer.

### Where the capability lives, and why it moved

An earlier draft put capabilities in the session, folded into each runner's slice. That draft could not express its own two most important capabilities.

`ask_user` and `submit_result` do not send the session anything. Their tool returns `StopRun`, the turn ends, and the meaning arrives later and indirectly as an outcome. So their capabilities' `handle` was unreachable code: no message ever arrived for them to claim. Worse, the ask's identity is a `tool_call_id` — a pointer into the agent's own transcript — so the one fact those capabilities needed was in a journal they could not write to.

The fix is not a new message type. It is that **a capability belongs in the actor whose state it needs**, and for every capability that is the agent. What is left in the session is not a capability at all: it is the runner tree, which is structure.

The test that settled it: under this split, `ask_user` involves the session **not at all** — not more honestly, not less often, but zero times. A design where the sharpest case needs no coordination is in the right place.

## Goals

- Any agent may create any number of subagents and workflow runs, nested arbitrarily.
- A workflow's graph is journal data, so an ad-hoc graph becomes possible without further structural change.
- A workflow session — a run with no conversation — keeps working, as a session whose root runner is a workflow.
- Deleting the per-kind matches: one dispatch by owner, not by kind.
- Session state carries structure and aggregates only; nothing that belongs to one agent.

## Non-goals

- The `invoke_workflow` tool itself, and ad-hoc graph construction. This spec makes both expressible; neither ships here.
- Multiple runtimes per session. The runtime becomes a runner, which makes this possible later; one is still the rule.
- The provider layer: the LLM call, retries, streaming. Capabilities are extensions to the loop, never the loop itself.

`AgentActor` **is** in scope, and that is a change from the first version of this spec. The capability machinery lives there now, so its state, its journal and its command handling all move. That was the cost of putting capabilities where their state is, and it is worth paying: it is what lets `ask_user` and `submit_result` be capabilities at all.

## Locked decisions

1. **One session, many runners.** An invoked workflow runs inside the session that invoked it. There is no child session.
2. **Concurrent runs share the session's workspace.** Runs proceed in parallel, each still running one step at a time. This is the contract subagents already have — one workspace, several writers.
3. **The journal shape breaks, on both sides.** State fields are renamed and merged and event variants are replaced, in the session's journal and the agent's. Existing sessions are truncated, not migrated.
4. **Capability state is agent state.** A capability is folded from its agent's journal. A runner holds no capabilities, and the session's journal carries none of their events. This is the decision every other one below follows from.
5. **The session has no capabilities.** It starts runners, forwards what people send, tracks the tree, and holds session-level facts — the title, the sandbox, usage by model. Anything an agent can *do* is a capability, and lives with the agent.
6. **Capabilities decide; the agent actor acts.** A capability returns events and requested acts. It never touches the mailbox, never spawns, never writes state. The one exception is `setup`, which is async and runs before the loop starts.
7. **A fact lives on exactly one side.** Two journals mean a fact recorded in both can disagree after a crash, with nothing to detect it. Questions belong to the agent; outstanding children belong to the agent that is waiting; the runner tree belongs to the session. Anything derivable is derived, never stored twice.
8. **The workspace scan is durable.** It is persisted in the runtime capability's slice rather than re-read every turn. Staleness is acceptable *because* the agent has a `scan_workspace` tool: it can notice and refresh. Today's per-turn scan is a sandbox round trip on every single turn.
9. **Plugins are per agent, not per session.** Each agent provisions its own bundles from its own settings and scans that tree, so a workflow step and the main agent already see different plugin libraries today. A subagent matches its caller only because it inherits the caller's settings — inheritance, not structure.

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

They live with the agent, not the session — `agent_loop/capabilities/`, beside the loop they extend:

```
agent_loop/
  capabilities/
    mod.rs                  Capability trait, Msg, Decision, Act, CapEvent, CapSlice
    runtime.rs  memory.rs  mcp.rs  control_plane.rs
    ask_user.rs  title.rs  sub_agent.rs  workflow.rs  fork.rs  step_result.rs

sessions/runners/
  mod.rs                    Runner trait, RunnerRecord, RunnerState, RunnerEvent
  conversation.rs  subagent.rs  workflow.rs  runtime.rs
```

Each file owns its capability's struct, its `Event`, and its request types. The module path supplies the namespace, so the inner types stay plain — `sub_agent::Event`, not `SubAgentCapabilityEvent`.

### What an agent actor is responsible for

Five things, and nothing else:

1. Driving the loop — the provider call, retries, streaming, compaction's mechanism.
2. Persisting its transcript **and its capabilities' state**, by the same event-sourcing fold.
3. Routing commands and events to its capabilities.
4. Dispatching loop lifecycle — turn started, ended, failed, stopped — to all of them.
5. Offering capabilities the few things only it can do: answer a call, park one, resume parked ones, enqueue into its own inbox, ask the session.

Point 5 is the whole of `Act`. If a capability needs something that is not on that list, either the list grows deliberately or the capability is reaching past its boundary.

### What a session actor is responsible for

1. Starting runners — the main conversation, subagents, workflow runs, forks.
2. Forwarding what people send to the agent that should get it.
3. Tracking the tree: which agent belongs to which runner, and each runner's status.
4. Session-level state only — the spec, the sandbox, the title, usage by model. Status and usage are the deliberate exception to "no per-agent state", and they are non-functional: nothing decides anything from them, they are what the list and the usage page read.

### The four stages

```rust
trait Capability {
    /// How progress names me while I set up, and how a test pins the set.
    /// A method rather than an associated const, which would make the trait
    /// not dyn-compatible.
    fn name(&self) -> &'static str;

    /// Once, when the agent is loaded. Acquire what this agent needs — a
    /// runtime client, an MCP connection — and fill in the spec.
    ///
    /// Async, and run on the *agent's own task*: acquiring a runtime can take
    /// thirty seconds, and the session's mailbox also carries Stop.
    async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError>;

    /// Once, when it unloads. Release what setup acquired.
    async fn teardown(&self);

    /// The tools I put in front of the model.
    ///
    /// One generic toolbox layer dispatches by name to whoever answers for it,
    /// so no capability needs a `Toolbox` of its own. This deletes
    /// `AskUserToolbox`, `StepResultToolbox`, `SubAgentToolbox` and
    /// `SessionTitleToolbox` — four types whose whole job was to turn one tool
    /// name into one message.
    fn tools(&self) -> Vec<ToolSpec> { Vec::new() }

    /// `None` means "not mine" — the next capability is offered it.
    ///
    /// One method rather than a `supports` predicate beside a handler: a
    /// capability that answered yes and then could not cope, and a pair edited
    /// out of step, are states that cannot be written this way.
    fn handle(&self, msg: &Msg) -> Option<Decision>;

    /// Fold my own durable slice, from *my agent's* journal. Pure.
    fn apply(&mut self, event: &CapEvent) {}

    /// Me, in the form the journal stores.
    fn save(&self) -> CapSlice;
}

/// Everything the agent actor routes to its capabilities.
enum Msg<'a> {
    Tool(&'a ToolCall),
    /// Loop lifecycle. Broadcast to every capability rather than offered until
    /// one claims: a turn ending is news for all of them, not a message with
    /// one owner.
    Turn(TurnEvent),
    Answer(&'a [AskAnswer]),
    /// A runner I started has news.
    Child(&'a ChildReport),
    /// The session answered a request of mine.
    Reply(&'a SessionReply),
}

/// Events for my agent's journal, acts for my agent actor.
struct Decision {
    events: Vec<CapEvent>,
    acts: Vec<Act>,
}

/// The five things only the agent actor can do. If a capability needs a sixth,
/// this list grows deliberately — it does not reach around.
enum Act {
    Answer  { call: String, text: String },
    /// Stop the run; this call stays open until something resumes it.
    Park    { call: String },
    Resume  { results: Vec<(String, String)> },
    Enqueue { item: Incoming },
    Ask(SessionRequest),
}

/// The whole agent -> session vocabulary, replacing six ad-hoc channels.
enum SessionRequest {
    StartRunner { kind: RunnerKind, args: RunnerArgs },
    Cancel      { agent: AgentId },
    SetTitle    { title: String },
}
```

`setup` is **per agent load, not per turn.** Today's code re-scans the sandbox on every turn; with the scan persisted (below) almost nothing is left that changes between turns. If something ever does, it gets an explicit `before_turn`/`after_turn` pair on this trait rather than a `setup` that quietly re-does itself — visible on the trait, or not happening.

`Park` and `Resume` are the two verbs the transport lacks today, and their absence is exactly why `ask_user` and `submit_result` could not be capabilities. A tool that returns a value could always be forwarded and answered; a tool that *parks* had no way to say so, so it hardcoded `StopRun` and let its meaning arrive later as an outcome. With these two, parking is something a capability's decision asks for rather than something a toolbox knows.

### One driver

The **agent actor** drives all of it — `setup` and `teardown` before the loop starts, `handle` and `apply` while it runs. There is no second driver and no copy to keep in step. The previous draft had two, and the seam between them is where `ask_user` fell through.

A capability still holds two kinds of thing: a **durable slice** (journalled in its agent's log, folded, replayed) and **turn resources** (the runtime client, the MCP connections — acquired in `setup`, released in `teardown`, never journalled).

### A runtime-composed list, not a closed enum

An agent holds `Vec<Box<dyn Capability>>`, built by `assemble` and carried in its `AgentSpec`. An agent that should not delegate simply is not given the capability, rather than holding one that refuses.

An earlier draft of this section accepted a cost that turned out not to be necessary: that the journal would have to carry `{ capability: name, event: … }` and route the fold by name, so a rename broke replay at runtime rather than at compile time. It does not. What has to be open is the *dispatch* — which capabilities a runner holds, and in what order. Persistence is a separate question, and it stays closed:

```rust
/// One capability as it is persisted. Serialising the whole capability, not a
/// durable-state extract, so a reload does not depend on `assemble`
/// reproducing the config it produced when the runner was created.
enum CapSlice { Runtime(..), Mcp(..), Memory(..), /* one arm each */ }

/// The list, round-tripping through `Vec<CapSlice>` with no hydration step.
struct Capabilities(Vec<Box<dyn Capability>>);
```

So the journal stays typed, a shape change still fails to compile, and `CapEvent` stays a closed enum too. `Capabilities::clone` goes through `save()` as well, which is what makes the copy an agent equips itself from provably the same thing a reload would build. `name()` and its pinning test remain, for narration and for event routing within a capability — not as a substitute for the compiler.

### Setup, and what the user sees while it runs

Setup runs in a written order, and the driver narrates it — emitting *starting* and *done* around each capability from its `NAME`, so no capability author has to remember to. A capability pushes its own detail for things only it knows, which is how a vendor's account of a booting machine already reaches the user.

```
  → "acquiring the runtime"      RuntimeCapability::setup
       the vendor's own narration arrives as detail
  → "scanning the workspace"     (same capability)
       reuses the persisted scan, or refreshes it; writes the workspace,
       the skills and the agent catalogue into the spec
  → "connecting mcp: github"     McpCapability::setup
  → "loading memory"             MemoryCapability::setup
  → "ready"
```

Order is also how a dependency between capabilities is expressed. `SubAgentCapability::setup` runs after the runtime's and reads the agent catalogue for its tool description, then keeps it for as long as the agent is loaded, so `handle` can reject an unknown agent type with a message naming what exists. No third party enriches anything: the capability that will answer the call is the one that took the list.

### The agent spec is the real thing

`setup` fills in an **`AgentSpec`**, and when every capability has run, the agent loop runs with it: the provider, the composed system prompt, the assembled toolbox, and the shared facts later capabilities read.

It is not a description that something else realises later. An earlier draft made it one, built at decision time and turned into a real toolbox by a nameless third party, and that third party was the tell.

One consequence: `Runner::actions` cannot hand a finished spec to `Action::StartAgent`, because building one is async. The action names the capability set; the agent's task builds the spec from it. Deciding *to* start an agent and *making* it ready were never the same act.

**Tool calls are offered around; structural messages are looked up.** A tool call goes through `capabilities.iter().find_map(|c| c.handle(from, &msg))`, which is what lets `Runtime` answer for a namespace nobody can enumerate — the sandbox toolbox plus whatever the plugin library scan discovered — while the fixed-name capabilities answer for theirs. A child's outcome and an arriving answer do **not** go through that scan: they route to the capability that created that child or recorded that ask, which is one owner by construction, recorded in that capability's own slice. Scanning there would let two capabilities plausibly claim the same `ChildOutcome`, which is the ambiguity most worth designing out.

Order is therefore the conflict resolution for tool calls, and it must be a written property of assembly rather than an accident of construction: the open-namespace capabilities — `RuntimeCapability`, `McpCapability` — sort last. This is the behaviour today, where `AskUserToolbox` wraps the plugin toolbox and silently shadows a plugin tool of the same name, so it is not a regression; but it is worth a debug-only assembly pass that offers a synthetic call to every capability and warns when more than one answers.

### Message

One enum with nested arms, so a capability's `handle` is a single match — and so the outer arm carries the dispatch rule rather than a comment carrying it.

```rust
enum Message {
    /// A tool the agent called. Offered around until one capability takes it.
    Tool(ToolCall),
    /// A `/builtin` the person typed, already parsed. Also offered around —
    /// `/fork` and `/compact` belong to different capabilities.
    Command(Invocation),
    /// A runner I created moved. Addressed: the owner is the capability that
    /// has this child in its own slice.
    Child(ChildMsg),
    /// Addressed: the capability holding the pending ask.
    Ask(AskMsg),
}

struct ToolCall { id: String, name: String, input: Value }

enum ChildMsg {
    /// It reached its end, already translated by the child into the
    /// vocabulary of whoever created it.
    Outcome { child: RunnerId, outcome: ChildOutcome },
    /// It is now runnable — a fork whose seed landed.
    Ready   { child: RunnerId },
    /// It never started: the create or the seed failed.
    Failed  { child: RunnerId, error: String },
}

enum ChildOutcome {
    SubAgent(SubAgentOutcome),   // Completed { label, report } | Failed { label, error }
    Workflow(WorkflowOutcome),   // Finished { output }         | Failed { error }
}

enum AskMsg { Answered { answers: Vec<AskAnswer> } }
```

The routing rule reads off the variant, so the session needs no table:

```rust
impl Message {
    fn routing(&self) -> Routing {
        match self {
            Self::Tool(_) | Self::Command(_) => Routing::Offer,
            Self::Child(m)                   => Routing::Owner(m.child()),
            Self::Ask(_)                     => Routing::PendingAsk,
        }
    }
}
```

`ChildOutcome` has no `Fork` arm. A fork owes nobody a result, so it reaches its creator through `Ready`/`Failed` only — the asymmetry stops being a special case and becomes a variant that simply is not there.

Who takes what:

| capability | takes |
|---|---|
| `RuntimeCapability` | `Tool(*)` — whatever the sandbox toolbox and plugin scan accept; sorts last |
| `McpCapability` | `Tool("mcp__…")` |
| `MemoryCapability` | `Tool("memory_*")` |
| `ControlPlaneCapability` | `Tool("horsie_*")` |
| `AskUserCapability` | `Tool("ask_user")`, `Ask(Answered)` |
| `TitleCapability` | `Tool("set_session_title")` |
| `SubAgentCapability` | `Tool("spawn_agent" \| "subagent_status")`, `Child(Outcome(SubAgent(_)))` |
| `WorkflowCapability` | `Tool("invoke_workflow" \| "workflow_status")`, `Child(Outcome(Workflow(_)))` |
| `ForkCapability` | `Command("/fork" \| "/summary-n-fork")`, `Child(Ready \| Failed)` |
| `StepResultCapability` | `Tool("submit_result")` |

One handler whole, to fix the shape:

```rust
impl Capability for SubAgentCapability {
    fn handle(&self, from: AgentId, msg: &Message)
        -> Option<(Vec<CapEvent>, Vec<Action>)>
    {
        match msg {
            Message::Tool(t) if t.name == "spawn_agent" => {
                // The tool's schema and this type are one declaration, so a
                // call whose arguments no handler accepts is unwritable.
                let req: sub_agent::Request = serde_json::from_value(t.input.clone()).ok()?;
                let child = RunnerId::new();
                Some((
                    vec![CapEvent::SubAgent(sub_agent::Event::Started { child, from })],
                    vec![Action::CreateChild {
                        kind: RunnerKind::SubAgent,
                        args: req.into_args(self.child_settings.clone()),
                        parent: from,
                    }],
                ))
            }
            Message::Tool(t) if t.name == "subagent_status" => { /* a read; no events */ }

            Message::Child(ChildMsg::Outcome { child, outcome: ChildOutcome::SubAgent(o) }) => {
                let to = *self.outstanding.get(child)?;   // None: not one of mine
                Some((
                    vec![CapEvent::SubAgent(sub_agent::Event::Reported { child: *child })],
                    vec![Action::Deliver { to, from: *child, part: o.into_part() }],
                ))
            }

            _ => None,
        }
    }
}
```

The `?` on `outstanding.get` earns its place: a child this capability did not create falls through as `None` rather than being mishandled, so "addressed by owner" is enforced by the same return type as "not my tool".

There is one implementation of each capability; per-runner variation is expressed at construction:

```rust
// capabilities/sub_agent.rs
struct SubAgentCapability {
    /// Fixed when the owning runner built this: what children inherit.
    child_settings: AgentSettings,
    /// Which child, and which of my agents asked for it. This one map says
    /// both "is a report still owed" and "who to deliver it to".
    outstanding: BTreeMap<RunnerId, AgentId>,
}

enum Event {
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
               ├── SubAgentCapability  { child_settings, outstanding }
               ├── AskUserCapability   { … }
               └── StepResultCapability{ … }
```

`CapEvent` is a closed enum with one arm per capability rather than an opaque blob: it keeps the journal typed, and a missing arm is a compile error in the right place.

**Instances belong to the runner; equipment is computed per agent.** A `WorkflowRunner` holds one `SubAgentCapability` and one `AskUserCapability`, because their state outlives any single step. Which capabilities a *given* agent is equipped with is decided at spawn, by folding a subset over its `AgentSpec`. That is how a workflow whose step 1 is interactive and step 2 is not gets exactly the right tools, with one mechanism rather than two — and it replaces the four-arm toolbox match and the four-arm prompt-suffix match in `context.rs`.

A message every capability declined is an error, never a silent drop — `None` from all of them, checked in the one place the scan lives. That check replaces an exhaustive-match compile error the current code relies on, so it is a real downgrade in safety; making it loud is the least that compensates.

## The agent lifecycle

Every runner that owns agents implements one handler; `RuntimeRunner` does not implement it at all, so "a runner with no agents cannot be handed an agent event" is a type fact rather than an unreachable arm.

```rust
/// Every method returns the same shape every decision in this design returns:
/// events to journal, and actions for the session to perform. Deliberately the
/// same shape as a capability's `Decision`, one level up — "decide, never
/// perform" is one idea, and two shapes for it would read as two.
struct Emit { events: Vec<RunnerEvent>, actions: Vec<Action> }

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
    /// Carries the ingredients, not a finished `AgentSpec`: building one is
    /// `setup`, which is async, so the agent's own task does it.
    StartAgent  { agent: AgentId, equipment: Capabilities, settings: AgentSettings, first: Incoming },
    CreateChild { kind: RunnerKind, args: RunnerArgs, parent: AgentId },
    Deliver     { to: AgentId, from: RunnerId, part: ResultPart },
    Cancel      { agent: AgentId },
}
```

One gate in front of all of it: nothing starts unless the `RuntimeRunner` is `Ready`.

## Routing

Two routings, one in each actor, and neither is a search.

**Inside an agent**, a message is offered to each capability until one claims it — except `Msg::Turn`, which is broadcast to all of them, because a turn ending is news for everyone rather than a message with an owner. A child's report is claimed by its `outstanding` gate rather than by an owner lookup somebody else performs, which means the agent actor needs no notion of who owns what.

**Inside the session**, everything an agent sends is addressed by the agent that sent it:

```rust
let runner = state.agents[&agent];
```

That single lookup replaces `on_agent_outcome`'s identity probing, `stop_target`'s three-registry walk, and the same walk in `resolve_agent` and `reach`.

Usage, turn-preparation progress and hook records mean the same thing for every runner; the session records those itself rather than routing them, so no runner grows a tail of variants it ignores.

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
  scan: R2 is Done, parent = A, and W's SubAgentCapability still lists R2
  R2::outcome() -> SubAgentOutcome::Completed { … }                 <- translation
  W.subagents.handle(ChildOutcome) -> (Reported { R2 }, Deliver { to: A })
  perform Deliver: Enqueue into A                                   <- tell
  PERSIST #2                                                        <- durable
```

One batch would leave no re-drive point. With two, a crash between them replays into "R2 is Done and W still lists it outstanding", so the boundary scan notices again. That is what makes `outstanding` the single fact recording both whether the parent has been told and which agent to tell — there is no separate `notified` flag to disagree with it.

## Recovery

Nothing separate is recovered. There is one journal — the session's. Replay folds every event, including `Runner(R, SubAgent(Started { child, from }))`, through `SessionState::apply_event`, which routes to `runners[R]`, which routes to the capability owning that arm. When the fold ends, every runner and every capability holds exactly what the log says.

The session then instantiates the runner impls from `RunnerRecord.kind` and calls `actions()` on each. That is the whole of recovery.

The only things outside the fold are live actor handles, which are connections rather than state, rebuilt on demand exactly as a cold subagent's actor is spawned on demand today.

## Invariants

1. A runner writes only its own slice. Cross-runner facts arrive as calls.
2. A runner returns events; `apply` is the only writer. Nothing mutates state directly.
3. A runner impl holds no fields. All state is in the session's fold.
4. `actions()` is pure and idempotent.
5. One runner owns exactly one agent role.
6. **An agent may not conclude while it has outstanding children.** Enforced where it is cheapest: the capability holding `outstanding` is the same one offered `Msg::Turn(Ended)`, in the same actor, with no coordination. Under the previous draft the fact lived in the session and the question was asked in the agent, which is why this invariant needed an enforcement point at all.
7. A child's report reaches exactly one capability, gated by its own `outstanding` — not by an owner lookup someone else performs. A tool call declined by every capability is an error.
8. Capability order is a written property of assembly — open-namespace capabilities last — not an accident of construction.
10. **A capability never reaches past `Act`.** Five verbs, and a sixth is added deliberately rather than worked around.
11. **Nothing is journalled twice.** Decision 7 restated as something a reviewer can check: for any fact, name the one journal that holds it.
9. `ChildOutcome` has no arm for a child that owes nothing. A fork reaches its creator through `Ready`/`Failed` only, so "a fork reports a result" is unwritable rather than checked.

## What this deletes

- `fork.rs` and `forks.rs` — a fork becomes a conversation with a branch point.
- The subagent forest in `subagents.rs` — `SubAgentForest`, `TreeOwner`, `owner_for`, `root_owner`.
- The three-registry probes in `resolve_agent`, `stop_target` and `reach`.
- The four-arm matches in `context.rs` for toolbox layers and prompt suffix, plus `build_memory_layer` and `build_control_layer` as special cases.
- `AgentKey` and `SessionAgentKind` — one flat `AgentId` space, owner resolved by lookup.
- `effective_settings` and `effective_settings_for_parent`.
- The defensive `TurnEnd::Asked`/`Parked` arms in `on_sub_agent_outcome`.
- `AskUserToolbox`, `StepResultToolbox`, `SubAgentToolbox`, `SessionTitleToolbox` — one generic layer dispatches by name to the capability that answers for it.
- `answered_turn` in `inbox.rs`, and `AgentState.asks` — the reconciliation moves to the capability that asked.
- The six ad-hoc agent-to-session channels, replaced by `SessionRequest`'s three verbs.
- `AgentOutcome`'s double life: turn lifecycle stays, `Asked` and `ForkSummary` become capability business, `UsageRecorded` becomes a report.

Roughly 3,400 lines net removed, against a rewrite of the two most load-bearing actors in the server.

## Open items

**What a spawn carries when the child's plugins differ.** An `agent_type` name only means something relative to a plugin set. Today the name is journalled bare and re-resolved by the child against *its* plugin tree, which works only while parent and child share one. They already need not, and the intended future — a spawn that overrides the child's model, skills, plugins or runtime — makes them deliberately different. A parent naming a plugin agent type means *its* plugin's agent, so the name alone is an under-specified handoff. Either the resolved definition travels with the spawn, or the plugin set does. Not settled.

**Concurrency cap — settled by the move.** The limit is a property of the sandbox, so the count belongs to the session, which owns the tree. But the capability asking is agent-side and cannot see that count. So the session enforces it when it is asked to `StartRunner`, and the refusal comes back as a `SessionReply` the capability turns into a tool result. The capability asks; the session says no; the model is told why.

That is better than either half alone. Today's per-caller cap is an artifact — a workflow session has no session-wide `AgentSettings`, so the number had to come from the step's preset, while the *count* was already session-wide. Putting the check where the count lives ends that mismatch. Per-runner sub-budgets can be added later as a value on the runner if fan-out starvation turns out to be real.

**A superseded step.** If a step agent's execution ends before a subagent it spawned finishes, delivery wakes an agent whose step is closed, and a second conclusion lands on an index the run already routed past. Invariant 6 is the answer, and it is now cheap: the capability holding `outstanding` is the same one offered `Msg::Turn(Ended)`, in the same actor, so the step simply does not conclude. Note this looks like a latent defect in the current code too: `owed_deliveries` deliberately routes to the superseded step (`orchestrator.rs`: *"a step that has since been superseded is still what asked"*), and `on_step_outcome` maps the second conclusion onto `StepConcluded { index }`, whose fold overwrites the entry's output and resets the run to `Running`. Worth a test before designing around it.

**Cancel cascade.** Cancelling a runner must cancel the runners parented on its agents, recursively — the same walk a depth budget uses. Not yet designed; retrying a step today would leave an invoked run orphaned with a dead parent agent.

**Recursion budget.** One depth number across the combined runner tree, replacing `MAX_SUBAGENT_DEPTH`. Without it, a workflow whose step invokes the same workflow has nothing bounding it.

**Delivery scan cost.** The boundary scan is over `runners where status == Done && parent.is_some()`, then a lookup in the parent's capability. Same shape as today's `owed()`; fine at tens of runners, worth indexing if a session ever holds hundreds.

## Testing

- Recovery: a journal cut mid-create replays to no child; cut between the two delivery batches replays to a re-delivered report.
- One runner, one agent role: a workflow's step agents all end through the same path.
- Capability dispatch: a tool call every capability declines errors rather than vanishing; a fixed-name capability wins over the open-namespace one that sorts after it.
- Nesting: a subagent of a step agent behaves identically to a subagent of a main agent, and a workflow invoked by a subagent delivers its terminal output to that subagent.
- Invariant 6: an agent that tries to conclude with outstanding children does not.

## Two flows, in full

The two capabilities that stress the split hardest: one that needs the session for nothing, and one that cannot proceed without it.

### `ask_user` — the session is not involved

```
model ──ask_user──▶ AgentActor
        offer Msg::Tool ─▶ AskUser claims
        events [Asked{call, question}]   acts [Park{call}]
        journal ▸ fold (pending += call) ▸ stop the run
        report status AwaitingInput ─▶ SessionActor   (status only, never the question)

person ──HTTP──▶ SessionActor ──forward──▶ AgentActor
        offer Msg::Answer ─▶ AskUser claims
        events [Answered]   acts [Resume{call → text}]
        journal ▸ fold ▸ fill the tool_results ▸ the turn continues
```

The questions never leave the actor that owns the transcript they point into. The session sees one status change. `answered_turn` and `AgentState.asks` both disappear, because the capability that asked is the one that reconciles.

### `spawn_agent` — the round trip, and its crash window

```
model ──spawn_agent──▶ AgentActor(parent)
        offer Msg::Tool ─▶ SubAgent claims
        events [Requested{call}]   acts [Ask(StartRunner), Park{call}]
        journal ▸ fold ▸ park ──────▶ SessionActor
                                       creates the runner and the child actor
                                       journals RunnerCreated
                     ◀──── SessionReply::Started{call, agent} ────
        offer Msg::Reply ─▶ SubAgent
        events [Spawned{child}]   acts [Resume{call → "Subagent spawned: {id}"}]
        journal ▸ fold (outstanding += child) ▸ the turn continues

  ... the child works, and concludes ...

AgentActor(child) ──outcome──▶ SessionActor ──ChildReport──▶ AgentActor(parent)
        offer Msg::Child ─▶ SubAgent, gated on its own `outstanding`
        events [Reported{child}]   acts [Enqueue(Incoming::SubAgent)]
        journal ▸ fold ▸ inbox — the next turn picks it up
```

**`Requested` is journalled before the ask, and that is the one price this design pays.** Two journals mean a crash between the ask and the reply leaves the session holding a runner the parent has never heard of. So the parent records its intent first; on load, a `Requested` with no `Spawned` is re-asked, and the session dedupes `StartRunner` by call id. Without that pair the window either loses a child or spawns it twice.

Note what invariant 6 costs here: nothing. The capability holding `outstanding` is the same one offered `Msg::Turn(Ended)`, in the same actor, folded from the same journal.

## Sequence reference

Two diagrams produced during design, kept for reference:

- Who talks to whom — subagent vs main agent vs step agents: <https://excalidraw.com/#json=wywPh4h34-krHI5wp1yT4,shNFa-M-FnxfQ3vSYnhZHA>
- A step agent spawning a subagent, end to end: <https://excalidraw.com/#json=EmuMLsI1ToBQmEfNO5GUO,oeF8Aac09Xwz3yyC4hTuGQ>
