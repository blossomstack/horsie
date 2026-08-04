# Workflows on the session server

**Date:** 2026-08-04
**Status:** approved

## The problem

`horsie` has three ways to put an agent to work, and none of them composes:

- **Agent presets** — a saved session configuration, invoked with a message.
- **Routines** — a preset plus a fixed prompt plus a trigger, producing one unattended session.
- **Subagents** — `spawn_agent`, where the *model* decides to delegate, up to depth 4.

What is missing is deterministic composition: *this* agent, then — depending on what it
concluded — *that* one, against a shared workspace, on a graph a person authored rather
than a model improvised.

The repository already contains an engine for exactly that. `workflow/src/workflow_actor.rs`
orchestrates a `WorkflowDefinition`: one agent per step, structured output per step,
conditional transitions between them, event-sourced. Its tests pass today
(`cargo test -p horsie-workflow`: 11 passed). But its only consumer is the `supervisor`
crate — the pre-server CLI job daemon — and **nothing depends on `supervisor`**. The
session server has never used it.

This design wires that capability into the server, the web UI, and the CLI.

## What a workflow is

A **workflow** is a named graph of steps. A **step** names an agent preset, carries a fixed
prompt, optionally declares a JSON Schema for its structured output, and lists transitions
to other steps guarded by conditions over that output.

A **run** of a workflow is a session. Not *like* a session — a session, with a session id,
in the session list, on the session API. Its steps are agents in that session's roster,
each with its own transcript at `/api/sessions/:id/agents/:agent_id`.

That single decision is what makes the feature small. A run inherits, with no new code:
one shared runtime (so step 2 sees step 1's edits on disk), idle offload, crash recovery,
the cancellation fence, the SSE stream, per-agent history and pagination, `ask_user` cards,
and `horsie session list/status/tail`.

### Division of concerns

| Belongs to | What |
|---|---|
| **The definition** | the graph: steps, prompts, output schemas, transitions, limits |
| **The invocation** | runtime vendor, repos, the run's input |
| **The step's preset** | model, MCP servers, memory spaces, thinking effort |

Where and against what a workflow runs is a property of running it, not of saving it. This
follows the argument in `2026-08-04-agent-skills-mcp-settings-design.md`: a pinned vendor is
invisible once it disconnects and fatal at invoke. Repos are the same — a definition that
hardcodes a checkout can only ever be run one way. And because a run *is* a session,
starting one takes exactly the configuration creating a session takes, so it reuses
`sessions/builder.rs` server-side and `SessionConfigBar` in the UI rather than growing a
second path.

A step's preset still carries `repos`; inside a workflow those are ignored, because the
workflow owns the one shared runtime.

> **Dependency.** This assumes `vendor` has left the agent preset, which is the in-flight
> `agent-skills-mcp` branch, not yet on `main`. Nothing here reads `AgentView.vendor`, so the
> two land in either order — but if that branch is abandoned, the run's vendor rule needs
> restating (a step's preset would then carry a vendor the workflow must ignore, exactly as
> it ignores `repos`).

## Architecture

### Why not host the existing `WorkflowActor`

`SessionActor::spawn_sub_agent_actor` (`session_actor.rs:585`) already does what
`WorkflowActor::spawn_agent` (`workflow_actor.rs:260`) does, but session-flavoured: it
builds a `SessionContextProvider` carrying the session's runtime, MCP, memory and provider
registry; spawns the `AgentActor` as its own child; registers it in the roster; and wires
the session's SSE frames and `SessionParent` outcome sink.

`WorkflowActor`'s version uses a `FixedContextProvider`, passes `Vec::new()` for MCP, has no
memory, no thinking effort, and its step agents are invisible to the session's roster, idle
offload, cancellation fence and `/sessions/:id/agents/:agent_id` reads — every one of which
this feature needs.

So `WorkflowActor` is retired, and with it the orphaned `supervisor` crate. What is kept is
what carries the value and is already shared:

- `WorkflowDefinition` / `WorkflowTransition` (reshaped — a step names a preset)
- `find_next_transition` and its `eval`-crate condition evaluation
- `WorkflowStatus` as the run's status model
- **the `conclude` tool and `output_schema` machinery** in `workflow/src/context.rs` and
  `AgentActor` — the real asset, already shared code the session simply switches on

### The orchestrator seam

The difference between an interactive session and a workflow run is entirely *what should
happen next*. Everything else — spawning, persisting, offload, runtime, frames, usage — is
shared. So the seam is a **pure decision trait**, and `SessionActor` remains the only thing
that performs effects.

```rust
/// Decides what a session does next. Pure — no actors, no I/O, no clock.
pub trait Orchestrator: Send + Sync {
    /// A resident agent reported a terminal outcome.
    fn on_outcome(&self, state: &SessionState, who: AgentKey, outcome: &AgentOutcome)
        -> Vec<SessionDomainEvent>;

    /// Anything to start? Called after every fold: inbox drain, step completion, recovery.
    fn next_action(&self, state: &SessionState) -> Option<AgentAction>;

    /// Which commands this session kind accepts.
    fn accepts(&self, cmd: SessionCommandKind) -> Result<(), &'static str>;
}

pub enum AgentAction {
    StartTurn { who: AgentKey, input: TurnInput },
    Finish { output: Value },
    Fail { error: String },
}
```

- `InteractiveOrchestrator` — inbox non-empty and no run in flight → `StartTurn{Main, inbox}`.
  Rejects `RetryStep`.
- `WorkflowOrchestrator` — holds the run's `WorkflowRunSpec`; on a step's `Concluded` it
  evaluates transitions and returns `StartTurn{Step(id), …}`, `Finish` or `Fail`. Rejects
  `UserMessage`.

`SessionActor` owns one, built at construction from `SessionSpec`, and its loop becomes:
fold events → `next_action` → perform it. Both orchestrators are unit-testable against a
hand-built `SessionState` with no actor, no runtime and no LLM, which is where all the
branching logic worth testing lives.

Two consequences that keep the trait pure:

- **Presets are resolved once, at run creation**, and snapshotted into `WorkflowRunSpec`
  alongside the definition. Editing a preset or the definition mid-run does not change the
  run.
- **Step agent ids are deterministic**: `Uuid::new_v5(session_id, "step:{index}")`. Replay
  reconstructs identical ids, so recovery resolves the same agent journals with nothing
  stored to keep in sync. Requires adding `"v5"` to the workspace `uuid` features (today
  `["v4", "serde"]`).

### Session state

```rust
pub struct SessionState {
    pub status: SessionStatus,          // unchanged: the ONE lifecycle truth
    pub pending_asks: Vec<PendingAsk>,
    pub inbox: Vec<InboxMessage>,
    pub last_error: Option<String>,
    pub agent_usage: HashMap<String, UsageTotal>,
    pub mode: SessionModeState,         // replaces `subagents`
}

pub enum SessionModeState {
    Interactive { subagents: SubAgentTree },
    Workflow(WorkflowRunState),
}

pub struct WorkflowRunState {
    pub status: WorkflowStatus,
    pub current: Option<usize>,
    pub steps: Vec<StepRun>,            // append-only: a loop or a retry appends
}

pub struct StepRun {
    pub step: String,
    pub agent: Uuid,                    // == the step's page
    pub attempt: u32,
    pub from: Option<usize>,            // the StepRun this came out of; None = start
    pub via: Option<String>,            // the transition condition that matched
    pub status: StepStatus,             // Running | Concluded | Failed | Cancelled
    pub settings: AgentSettings,        // preset resolved at run creation
    pub input: String,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub subagents: SubAgentTree,        // each step roots its own tree
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
}
```

**Steps are not subagents, and each step roots its own subagent tree** — a step agent can
spawn subagents exactly like a main agent can. `SubAgentTree` is reused verbatim: in an
interactive session its root is the main agent, in a step it is the step agent.

> `SubAgentParent::Main` must **not** be renamed to `Root`, even though that now reads
> better. It is a persisted enum, and renaming persisted variants is what killed the
> supervisor on the homelab and forced a session wipe. It keeps its name, with a doc comment
> saying it means "this scope's root agent".

Runtime side, the `main_agent: Option<…>` ambiguity goes away — today its `None` means "the
instant before spawn", and a workflow session would make it permanent:

```rust
enum SessionAgents {
    Interactive { main: ActorRef<AgentCommand>, subs: HashMap<Uuid, ActorRef<AgentCommand>> },
    Workflow    { live: HashMap<Uuid, ActorRef<AgentCommand>> },
}
```

`AgentKey` and `SessionAgentKind` both gain `Step(Uuid)`. A step behaves like `Sub` for
`scoped_client` (its own cwd/env bucket — steps must not share one) and like `Main` for
progress broadcast (a step is the visible work). It gets `conclude` with its `output_schema`,
memory and MCP from its own preset, the subagent toolbox, and `ask_user` when the run is
attended — never the title tool.

`SessionContextProvider.settings` moves from "the session's" to "this agent's", read from
`StepRun.settings` for a step.

**One status machine.** `SessionStatus` stays the single lifecycle truth. `WorkflowStatus`
lives inside `WorkflowRunState` and describes the run's shape. They cannot disagree because
both derive from the same event fold.

### Module layout

`session_actor.rs` is already 3,689 lines; none of this goes in it.

```
server/src/sessions/orchestrator.rs      the trait, AgentAction, InteractiveOrchestrator
server/src/sessions/workflow/mod.rs      WorkflowRunState, StepRun, StepStatus (data + fold)
server/src/sessions/workflow/driver.rs   WorkflowOrchestrator: transitions, retry, terminal
server/src/sessions/workflow/spec.rs     WorkflowRunSpec: definition snapshot + input
server/src/workflows/{mod,store,service}.rs   definition CRUD (mirrors agents/, routines/)
server/src/http/workflows.rs             handlers
```

## Wire types

`models/fluorite/workflow.fl` is reshaped in place — its only consumer, `supervisor`, is
deleted by this change.

```rust
struct WorkflowTransition {
    to: String,
    /// Expression over the producing step's structured output, bound to `output`.
    /// None is an unconditional catch-all. Transitions are tried in order, first match wins.
    condition: Option<String>,
}

struct WorkflowStepDef {
    name: String,
    /// Agent preset this step runs as: model, MCP servers, memory spaces, thinking effort.
    /// Its `repos` are ignored — the workflow owns the one shared runtime.
    agent: String,
    /// The step's instruction. Incoming data is appended below it.
    prompt: String,
    /// JSON Schema. Present → the step finishes via `conclude` with conforming output.
    /// Required when the step has any conditional transition.
    output_schema: Option<Any>,
    transitions: Option<Vec<WorkflowTransition>>,
    max_iterations: Option<u32>,
    max_retries: Option<u32>,
}

struct WorkflowView {
    name: String,
    description: String,
    start: String,
    steps: Vec<WorkflowStepDef>,
    created_at: String,
    updated_at: String,
}

struct WorkflowInput {
    name: String,
    description: Option<String>,
    start: String,
    steps: Vec<WorkflowStepDef>,
}

/// Start a run. The same configuration creating a session takes, plus the input.
struct WorkflowRunRequest {
    input: String,
    vendor: Option<String>,
    repos: Option<Vec<RepoConfig>>,
    name: Option<String>,
}

struct WorkflowRunResponse { session: SessionSummary }
```

Dropped from the old `WorkflowAgentDef`: `model`, `system_prompt` and `use_plugins` (now the
preset's or the workflow's), and `allowed_tools`, `allow_ask_user`, `allow_timers` — those
are decisions rather than configuration. `allow_ask_user` follows whether the run is
attended; timers stay off, as they are in sessions today.

### A step's input

A step receives its fixed `prompt`, with the incoming data appended below it under a header:

```
Review the change the previous step made.
Reject anything without a test.

## Input from step `fix`
{"files_changed": 3, "summary": "…"}
```

The start step receives the run's `input` under `## Input`. There is no template language:
transitions already provide the one expression surface, and a second one would need its own
escaping story, error handling and documentation.

### The run projection

The run log is a `Vec`, because the persisted shape must be a replayable log — append-only
is what makes `apply_event` a pure fold, and a graph with loops is not a tree. It projects
losslessly to a graph because every entry records where it came from (`from`, `via`).

```rust
struct WorkflowRunGraph {
    workflow: String,
    status: WorkflowStatus,
    current: Option<u32>,
    nodes: Vec<RunNode>,       // one per DEFINITION step — unvisited ones render greyed
    edges: Vec<RunEdge>,       // one per DEFINITION transition
    output: Option<Any>,
    error: Option<String>,
    usage_total: UsageTotal,
}
struct RunNode { step: String, runs: Vec<StepRunView> }
struct RunEdge { from: String, to: String, condition: Option<String>, traversals: Vec<u32> }
struct StepRunView {
    index: u32,
    step: String,
    agent_id: String,          // → /sessions/:id/agents/:agent_id — the click target
    attempt: u32,
    status: StepRunStatus,
    output: Option<Any>,
    error: Option<String>,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    usage: UsageTotal,
}
```

A step visited three times is one node with three runs; the loop itself is carried by the
edges' `traversals`.

## HTTP

```
GET    /api/workflows                      list
POST   /api/workflows                      create
GET    /api/workflows/:name                get
PUT    /api/workflows/:name                replace
DELETE /api/workflows/:name                delete
POST   /api/workflows/:name/runs           start a run → SessionSummary
GET    /api/workflows/:name/runs           this workflow's runs

GET    /api/sessions/:id/workflow          the run graph
POST   /api/sessions/:id/workflow/retry    { stepIndex }
```

Reused unchanged: `POST /api/sessions/:id/stop` (interrupt), `DELETE /api/sessions/:id`
(delete the run), `/events`, `/agents/:agent_id{,/history,/events}`.

`POST /api/sessions/:id/messages` returns **409** on a workflow session. That is
`Orchestrator::accepts` surfacing, so the rule lives in one place rather than in a handler
guard.

### Semantics

- **`SessionOrigin::Workflow { workflow }`**, and `SessionSummary` gains `origin`. Unlike
  routine runs, workflow runs are **not** hidden from the session list — they appear
  alongside regular sessions, annotated.
- **Deleting a definition** is a **409** while any of its runs is active. Finished runs
  survive it: each carries its own snapshot and is still listed.
- **Retry of step *i*** appends a new `StepRun` with the same `step` and `from`, and
  `attempt = prior + 1`. It never truncates; earlier attempts stay in the log and render as
  extra `runs` on that node. If the run is live, retry cancels the current step first via
  the existing cancel fence. **The shared workspace is not rolled back** — a retried step
  re-runs against whatever the failed attempt left on disk. This goes in the guide.
- **Validation at save** returns 422 naming the offending step: `start` names a real step,
  every transition target exists, no step name repeats, a step with a conditional transition
  has an `output_schema`, and every referenced preset exists.

### Plugins

A run provisions the **union** of the plugin bundles named by every step's preset, resolved
at run creation. `runtime_manager.rs` resolves bundles from `SessionSpec.plugins` once, at
provision ("one caller, once per session"), so with one shared runtime there is no
alternative.

Two accepted consequences, tracked in **#182**:

1. **Precedence** — if any single step's preset names a bundle, the union is non-empty and
   the host `--plugins-dir` library is replaced for the whole run, including for steps whose
   presets named none. Documented in the guide; no code.
2. **Visibility** — every step can reach every bundle in the union on disk. Filtering per
   step is possible (`PluginSkill` carries a `plugin` field) but needs a new argument
   threaded through `ToolboxFactory::for_agent`, so it is deliberately out of the first
   implementation.

MCP servers and memory spaces need none of this: both are composed server-side and never
touch the runtime, so they stay genuinely per-step.

## Web UI

Layout is a **rank assignment by BFS from `start`** (~40 lines, pure, unit-tested), rendered
as plain SVG; back-edges draw as curves to a lower rank. No graph library is added — these
graphs have well under twenty nodes. `dagre` is the fallback if hand-rolled layout proves
ugly on a real definition.

```
pages/workflows/WorkflowsPage.tsx       list + New
pages/workflows/WorkflowEditPage.tsx    step-list form, live graph beside it
pages/workflows/WorkflowDetailPage.tsx  definition + this workflow's runs + Run
pages/workflows/WorkflowRunView.tsx     the run page
components/WorkflowGraph.tsx            ONE renderer: editor preview and run view
lib/graphLayout.ts                      rank layout, pure
hooks/useWorkflows.ts                   CRUD, mirrors useRoutines.ts
hooks/useWorkflowRun.ts                 the run graph, refreshed off the session stream
```

Routes, alongside the existing `routines/*` block:

```
/workflows   /workflows/new   /workflows/:name   /workflows/:name/edit
/sessions/:id                   → SessionView, branches on origin
/sessions/:id/agents/:agentId   → the step page (also serves subagents)
```

**Sidebar.** A workflow run renders in the ordinary session list with a badge naming its
workflow. One conditional, driven by the new `origin` field.

**Run page.** `SessionView` sees `origin: Workflow` and renders `WorkflowRunView`.

- Header: status pill (`StatusBadge`), token total, Interrupt, Delete. Explicitly **no
  `ContextGauge`** — a run has no single context window.
- Body: the graph. Every definition step is a node; unvisited ones render greyed; a node
  that ran more than once shows its attempts stacked, latest on top. Node actions: open
  (→ its step page), retry, and interrupt when it is the live one.
- An `AwaitingInput` run surfaces the step's `AskUserCard` on the run page itself, posting
  to the existing `POST /api/sessions/:id/answers`.

**Step page.** The session page pointed at one agent. `useSessionStream` hardcodes
`MAIN_AGENT` in exactly two places (`useSessionStream.ts:548` and the history seed at :554);
it takes an `agentId` parameter instead. The step page is the same shell minus the composer
(a step takes no messages) and minus Delete; Interrupt and Retry stay. The same route serves
subagents, which have no page today.

**Editor.** Step cards in a list with the graph beside them. Each card: name, agent-preset
picker (reusing `configPickers`), prompt textarea, output fields, and transitions as
`[condition] → [target ▾]` rows. Save is a full `PUT`, matching agents and routines, with the
422 rendered against the offending card.

Output schema authoring is deliberately narrow: a **field list** (name, type, optional
description) compiling to a flat JSON Schema object — not a schema editor. Conditions like
`output.severity == "p0"` read naturally against flat objects, and the control can widen
later.

**Run dialog.** Because a run takes the configuration a session takes, the Run button opens
the existing `SessionConfigBar` pickers — runtime and repos — plus the input textarea.

## CLI

```
horsie workflow list
horsie workflow get <name>
horsie workflow run <name> --input <text> [--vendor <v>] [--repos <r>]   → prints the session id
horsie workflow status <session-id>                                      → the graph as a table
horsie session tail <session-id> [--agent <agent-id>]                    → the one addition
```

`session list/status` already work on runs. `session tail` streams
`/api/sessions/:id/events`, the session-scoped stream, so it works on a run unchanged; it
carries step transcripts only with `--agent`.

## Failure and recovery

**Recovery.** Sessions already recover lazily by folding the journal, and a run adds
nothing: `next_action` re-derives what to do. A step that was mid-run when the process died
goes through the existing `ReconcileInterrupted` path — that step becomes
`Failed { "interrupted by restart" }` and the run goes **Suspended, not resumed**. An
interrupted step's effect on the shared workspace is unknown, so a person decides between
retry and abandon.

**Step failure never auto-retries.** `max_retries` remains the provider retry budget inside
`AgentActor`. A step that fails terminally fails the run, with the error on its node.
Automatic retry against a shared mutable workspace re-does half-finished work.

**Two defects inherited from `workflow_actor.rs`, fixed rather than ported:**

1. `find_next_transition` (`workflow_actor.rs:675`) logs a warning and treats an
   unevaluatable condition as non-matching, so a typo in `output.severty == "p0"` silently
   falls through to the next transition or ends the run as if it had succeeded. For a
   user-authored expression that is the wrong default: it becomes a run failure,
   `condition 'X' on step 'Y' failed to evaluate: <err>`.
2. Nothing bounds the number of steps, so a loop whose condition never flips runs forever.
   A run gets a `max_steps` budget (default 100) → `Failed { "step budget exhausted after
   100 steps" }`.

**No transition matches** → the run finishes with that step's output. That is the existing
rule and it is how a terminal step ends.

**Asks.** A run started from the UI or API is attended, so its steps get `ask_user`. A
routine triggering a workflow would be unattended and must not — out of scope here, but the
`is_unattended()` seam already carries it.

## Testing

- **Pure:** `graphLayout.ts`; both orchestrators against hand-built `SessionState` with no
  actor, runtime or LLM — where all the branching worth testing lives; transition
  evaluation; save-time validation.
- **Actor, with `mock-llm`:** a two-step run routes on a condition; a step's ask pauses then
  resumes; retry appends an attempt; recovery mid-step lands Suspended.
- **HTTP e2e** in `tests/tests/session_server_e2e.rs` — serial against one long-lived
  server, so assert against baselines, never zero, and scope locators by `data-*`.
- **Playwright:** a workflow spec against the non-provisioning `e2e` vendor.

Two traps this codebase has already been bitten by:

- `AppState` is constructed in **three** places (`http::mod` tests, `main.rs`, and twice in
  the e2e suite); all need the new service.
- Adding a `.fl` file needs **four** edits: the schema, a `pub mod` in `models/src/lib.rs`,
  and the input list in **both** `clients/ts/package.json` and `clients/web/package.json`.
  CI only drift-checks `clients/ts`, so a missed web regeneration is silent.

## Delivery

| PR | Scope | Why separate |
|---|---|---|
| 1 | Session-actor refactor alone: `Orchestrator` seam, `SessionAgents` enum, `SessionModeState` with only `Interactive`. No workflows. | The one risky change, landing green with zero behaviour change |
| 2 | `workflow.fl` reshape, store + service, HTTP CRUD/run/graph, `WorkflowOrchestrator`; delete `supervisor` and `WorkflowActor` | The engine |
| 3 | Web: editor, run page, step page, sidebar badge, `useSessionStream(agentId)` | |
| 4 | CLI and `docs/guide/workflows.md` | |

## Out of scope

- **Parallel steps.** Every transition picks exactly one successor. Fan-out and join are a
  real want, but they change the run from a walk to a frontier, and the subagent tree
  already covers "do these three things at once" within a step.
- **Routine-triggered workflows.** The `is_unattended()` seam carries it; nothing else does
  yet.
- **A template language for step prompts.** Deliberately declined above.
- **Per-step plugin filtering.** Tracked in #182.
