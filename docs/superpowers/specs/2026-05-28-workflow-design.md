# Workflow Crate Design

**Date:** 2026-05-28  
**Status:** Implemented

## Overview

Two new crates added to the workspace: `actor` and `workflow`. Together they implement
multi-agent orchestration with event-sourced crash recovery, per-agent tool permissions,
and first-class support for user interruption and interaction.

```
agentcore   ← pure agent loop (unchanged)
actor       ← generic event-sourced actor runtime
workflow    ← WorkflowActor + AgentActor, uses actor + agentcore
models      ← existing; gains workflow.fl fluorite schema
```

---

## 1. Crate: `actor`

Generic, domain-free runtime. Neither `workflow` nor `agentcore` concepts appear here.

### 1.1 Core trait

```rust
pub trait EventSourcedActor: Send + 'static {
    type Command: Send + 'static;
    type Event:   Send + Serialize + DeserializeOwned + 'static;
    type State:   Send + Serialize + DeserializeOwned + 'static;

    fn persistence_id(&self) -> String;
    fn initial_state() -> Self::State;

    /// Pure — used both during event replay and live operation.
    fn apply_event(state: Self::State, event: Self::Event) -> Self::State;

    /// Async — may spawn children or tell other actors; returns what to persist.
    async fn handle_command(
        &mut self,
        state: &Self::State,
        cmd:   Self::Command,
        ctx:   &mut ActorContext,
    ) -> CommandEffect<Self::Event, Self::State>;

    /// Optional hook called once after full recovery before first command.
    async fn on_recovery_complete(
        &mut self,
        _state: &Self::State,
        _ctx:   &mut ActorContext,
    ) {}
}

pub enum CommandEffect<E, S> {
    Persist(Vec<E>),            // persist events, apply to state
    PersistAndSnapshot(Vec<E>), // persist events + save resulting state as snapshot
    Snapshot,                   // save current state as snapshot, no new events
    None,                       // no change
    Stop,
    PersistAndStop(Vec<E>),
}
```

### 1.2 ActorRef and ActorContext

```rust
pub struct ActorRef<C> { /* tokio mpsc sender */ }

impl<C: Send + 'static> ActorRef<C> {
    pub async fn tell(&self, cmd: C) -> Result<()>;
}

pub struct ActorContext { /* runtime handle */ }

impl ActorContext {
    /// Spawn a child actor. Inherits the journal; keyed by child's persistence_id.
    pub fn spawn<A: EventSourcedActor>(&self, actor: A) -> ActorRef<A::Command>;
    pub fn self_ref<C: Send + 'static>(&self) -> ActorRef<C>;
    /// Journal access for actors that need it directly (e.g. WorkflowActor for fork).
    pub fn journal(&self) -> &Arc<dyn Journal>;
}
```

### 1.3 Journal trait

```rust
pub trait Journal: Send + Sync + 'static {
    async fn persist(&self, id: &str, events: &[Bytes]) -> Result<()>;
    async fn replay(&self, id: &str, after_seq: u64) -> BoxStream<'_, Result<Bytes>>;

    async fn save_snapshot(&self, id: &str, state: Bytes, seq_nr: u64) -> Result<()>;
    async fn latest_snapshot(&self, id: &str) -> Result<Option<(Bytes, u64)>>;
    async fn delete_events_before(&self, id: &str, seq_nr: u64) -> Result<()>;

    async fn copy_snapshot(&self, from_id: &str, to_id: &str) -> Result<()>;
    async fn clear(&self, id: &str) -> Result<()>; // test helper
}
```

### 1.4 Recovery flow

On `spawn_root(actor, journal)` or `ctx.spawn(actor)`:

1. Call `latest_snapshot(persistence_id)` → `(state, seq_nr)` or `(initial_state(), 0)`.
2. Call `replay(persistence_id, after_seq: seq_nr)` → stream of serialized events.
3. Fold each through `apply_event` to rebuild `State`.
4. Call `on_recovery_complete(state, ctx)`.
5. Begin accepting commands.

An in-memory `Journal` impl ships in this crate for tests. Production impls
(SQLite journal, object-store snapshots) are future work in separate crates.

---

## 2. Crate: `workflow`

### 2.1 Runtime context types

Injected resources are grouped into named context objects. They are **not** persisted —
they are recreated on every spawn or restart.

```rust
/// Resources injected into WorkflowActor at construction.
pub struct WorkflowRuntimeContext {
    pub provider_registry: HashMap<String, Arc<dyn LlmProvider>>,
    pub toolbox_factory:   Arc<dyn ToolboxFactory>,
    pub runtime_client:    Arc<dyn RuntimeClient>,
    // RuntimeClient: abstraction over october's executor/runtime infrastructure;
    // passed into ToolboxFactory so tools can execute shell/file operations inside
    // a managed runtime. Concrete impl wraps the executor WebSocket protocol.
    pub event_sink:        Arc<dyn EventSink>,
    // Journal is NOT here — accessed via ActorContext.journal() instead.
}

/// Resources injected into AgentActor at spawn time by WorkflowActor.
pub struct AgentRuntimeContext {
    pub provider:   Arc<dyn LlmProvider>,
    pub toolbox:    Arc<dyn Toolbox>,        // pre-filtered for this agent
    pub event_sink: Arc<dyn EventSink>,
    pub parent_ref: ActorRef<WorkflowCommand>,
    pub session_id: Uuid,
}
```

`WorkflowActor` constructs `AgentRuntimeContext` at spawn time: resolves the provider
from `provider_registry`, calls `toolbox_factory.for_agent(agent_def, runtime_client)`,
and passes `ctx.self_ref()` as `parent_ref`.

### 2.2 Tool permissions

```rust
pub trait ToolboxFactory: Send + Sync + 'static {
    fn for_agent(
        &self,
        agent_def:      &WorkflowAgentDef,
        runtime_client: Arc<dyn RuntimeClient>,
    ) -> Arc<dyn Toolbox>;
}
```

`WorkflowAgentDef.allowed_tools: Option<Vec<String>>` — `None` means all tools;
`Some(list)` is an allowlist. The factory filters the full tool set down to what the
agent is permitted to use.

### 2.3 WorkflowDefinition (fluorite schema `workflow.fl`)

```
struct WorkflowAgentDef {
    name:           String,
    system_prompt:  Option<String>,
    handoff_tool:   Option<String>,      // tool name that triggers a transition
    max_iterations: Option<u32>,
    max_retries:    Option<u32>,         // retry budget for transient provider errors
    model:          String,              // key into provider_registry
    allowed_tools:  Option<Vec<String>>, // None = all; Some = allowlist
}

struct WorkflowTransition {
    from: String,   // agent name
    to:   String,   // agent name
    on:   String,   // handoff tool_name that fires this edge
}

struct WorkflowDefinition {
    start:       String,
    agents:      Vec<WorkflowAgentDef>,
    transitions: Vec<WorkflowTransition>,
}
```

### 2.4 WorkflowActor

```
Command:  Start { input: String }
        | Cancel
        | Resume { message: String }
        | Fork { from_session_id: Uuid, message: String }
          // ↑ creates new AgentActor inheriting history from from_session_id
        | _AgentHandoff { session_id: Uuid, tool_name: String, data: Value }
          // ↑ agent returned AgentResult::Handoff — look up transition by tool_name
        | _AgentDone    { session_id: Uuid, text: String }
          // ↑ agent returned AgentResult::Completed — no transition, workflow finishes
        | _AgentFailed  { session_id: Uuid, error: String, recoverable: bool }
        | _AgentPaused  { session_id: Uuid, tool_call_id: String, question: String }

Event:    WorkflowStarted
        | AgentStarted    { agent_name: String, session_id: Uuid, input: String }
        | AgentTransitioned { from: String, to: String,
                              from_session: Uuid, to_session: Uuid, on: String }
        | WorkflowFinished  { output: Value }
        | WorkflowSuspended
        | WorkflowFailed    { error: String, recoverable: bool }
        | WorkflowPaused    { session_id: Uuid, tool_call_id: String }

State:    WorkflowState {
              status:             WorkflowStatus,
              current_agent:      Option<String>,
              current_session_id: Option<Uuid>,
          }
```

`apply_event` is pure — each event drives a `WorkflowStatus` transition.

Snapshot after each `AgentTransitioned` or `WorkflowFinished` — keeps the event log short.

**Transition logic:**
- `_AgentHandoff { tool_name, data }`: validate `tool_name` against `WorkflowDefinition.transitions`
  (runtime check — error if no matching edge). If match → `PersistAndSnapshot([AgentTransitioned,
  AgentStarted])` + spawn next `AgentActor`. If no match → runtime error, workflow fails.
- `_AgentDone { text }`: no transition lookup. → `PersistAndStop([WorkflowFinished { output: text }])`.

### 2.5 AgentActor

```
Command:  Run { input: String }
        | Cancel
        | InjectToolResult { tool_call_id: String, content: String }

Event:    InputMessage    { message: Message }
        | MessageComplete { message: Message }
        | ToolComplete    { tool_call_id: String, output: String, is_error: bool }
        | RunComplete     { usage: Usage, iterations: u32 }
        | RunCancelled

State:    Vec<Message>   — conversation history
```

Streaming observation events (`TextChunk`, `ToolCallInputDelta`, etc.) are emitted to
`AgentRuntimeContext.event_sink` for real-time display but are **never journaled**.
Only the four coarse events above alter the persisted state.

`apply_event` reconstructs `Vec<Message>`:
- `InputMessage`    → push input as User or ToolResult message
- `MessageComplete` → push assistant message
- `ToolComplete`    → push Tool message with ToolResultPart
- `RunComplete`     → no-op on state (metadata only)
- `RunCancelled`    → no-op on state

Snapshot after `RunComplete` and after `RunCancelled` — full `Vec<Message>` replaces
all prior events; prior events are deleted from the journal. Snapshotting on cancel
ensures every session — whether it completed normally or was stopped mid-run — has a
clean fork point available.

**On completion**, `AgentActor` inspects `AgentOutput`:
- `AgentResult::Handoff { tool_name, data }` → tells `parent_ref`:
  `_AgentHandoff { session_id, tool_name, data }`
- `AgentResult::Completed { text }` → tells `parent_ref`:
  `_AgentDone { session_id, text }` — no transition, workflow finishes.

---

## 3. Error and interruption model

### 3.1 State machine

```
                   ┌─────────────┐
              ┌───►│   Running   │◄────────────────┐
              │    └──────┬──────┘                 │
   Resume/    │           │                        │
   user reply │    ┌──────┴────────────────┐       │
              │    ▼                       ▼       │
              │  User         Agent/     Logic     │
              │  cancel       provider   error     │
              │    │          error                │
              │    ▼          │                    │
              │  Suspended ◄──┘ (recoverable)      │
              │    │          │ (exhausted)         │
              └────┘          ▼                    │
                           Failed                  │
                        (recoverable) ─────────────┘
                        (terminal)   → end
                           
    Agent asks user:
    Running ──ask_user tool──► AwaitingUserInput
                                     │
                              user sends Resume(msg)
                                     │
                                  Running
```

### 3.2 Scenarios

**Agent logic error** (`MaxIterationsExceeded`, `StuckInLoop`):
- Classified `recoverable: false`.
- `AgentActor` persists `RunComplete`, tells `parent_ref _AgentFailed`.
- `WorkflowActor` persists `WorkflowFailed { recoverable: false }` → terminal.

**Provider/network error** (`LlmError`):
- Retry with exponential backoff up to `agent_def.max_retries`.
- If exhausted: classified `recoverable: true` → `WorkflowActor` → `Suspended`.
- Operator can send `Resume` to retry.

**Process crash / server restart** (handled by event sourcing):
- `WorkflowActor` recovers from journal → knows `current_session_id`.
- Spawns new `AgentActor` for that session.
- `AgentActor` loads snapshot → recovers `Vec<Message>` → resumes from last checkpoint.
- The in-flight LLM call is simply retried; no special handling needed.

**Fork** (redirect a workflow that went off track):
- Use case: user watches the agent going in the wrong direction, cancels the current
  session, then forks from any prior session (the cancelled one or an earlier completed
  one) and injects a correction message. The workflow continues sequentially from that
  point — no parallel execution.
- Only valid in `Suspended` state (the user must cancel first).
- `Fork { from_session_id, message }`:
  1. `WorkflowActor` calls `ctx.journal().copy_snapshot(from_session_id, new_session_id)`.
  2. Spawns new `AgentActor` with `persistence_id = new_session_id`.
  3. New actor recovers from the copied snapshot (history = old `Vec<Message>`); no
     events to replay since `new_session_id` has none yet.
  4. `WorkflowActor` persists `AgentStarted` with `new_session_id`; tells new actor
     `Run { input: message }` — agent continues with the correction injected.
- The `Journal` trait gains one method to support this:
  `async fn copy_snapshot(&self, from_id: &str, to_id: &str) -> Result<()>;`

**User cancel** (not a failure):
- `Cancel` → `WorkflowActor` tells `AgentActor Cancel`.
- `AgentActor` fires `CancellationToken` → agent returns `AgentError::Cancelled`.
- `AgentActor` persists `RunCancelled`, stops.
- `WorkflowActor` persists `WorkflowSuspended` → status `Suspended`.
- `Resume(msg)` later → spawns new `AgentActor` with recovered history.

**User interaction / ask** (first-class workflow state):
- Agent calls `ask_user` tool (a designated tool in its toolbox).
- `AgentActor` captures `tool_call_id`, tells parent `_AgentPaused { question }`.
- `WorkflowActor` persists `WorkflowPaused { tool_call_id }` → status `AwaitingUserInput`.
- `Resume(message)` → `WorkflowActor` tells `AgentActor InjectToolResult { tool_call_id, message }`.
- Agent continues with the user's reply as the tool result.

---

## 4. Testing strategy

**`actor` crate** — unit tests using in-memory `Journal`:
- Recovery after simulated crash (persist events, create new actor, verify state).
- Snapshot compaction (persist N events, snapshot, verify replay skips old events).

**`workflow` crate** — unit tests with mock providers:
- `WorkflowActor` state machine: each command/event pair tested in isolation.
- `AgentActor` history reconstruction: verify `apply_event` rebuilds `Vec<Message>` correctly.
- `WorkflowActor` transition routing: correct next agent selected by `tool_name`.

**`tests/` integration** — full-stack using `mock-llm`:
- Two-agent sequential workflow: verify `AgentTransitioned` event and correct input passing.
- Cancel and resume: verify `WorkflowSuspended` then `Running` after `Resume`.
- Ask/reply cycle: verify `AwaitingUserInput` state and reply injection.
- Crash recovery: persist partial state, reconstruct actors, verify workflow continues.

---

## 5. Implementation notes

Decisions made while implementing the spec, recorded here so the design matches
the code:

- **`ActorContext<A>` is generic over the actor**, so `self_ref()` returns a typed
  `ActorRef<A::Command>` checked at compile time rather than a downcast
  `self_ref::<C>()`. This follows the project's compile-time-over-runtime rule.
- **`CommandEffect<E>` drops the unused `S` parameter** — no variant carried state,
  so the second type parameter was dead.
- **Journal blobs are `Vec<u8>`** (not `Bytes`); events/state are serialized with
  `serde_json`. Sequence numbers survive compaction (stored per event), so a
  forked or recovered actor continues numbering correctly.
- **`WorkflowResumed` event added.** `apply_event` is pure, so the
  `AwaitingUserInput`/`Suspended` → `Running` transition needs an explicit event.
- **`AgentActor` runs the agent loop on a background task** and reports completion
  to itself via an internal `RunFinished` command. This keeps the mailbox free so
  a `Cancel` can interrupt an in-flight run between iterations.
- **Cancel keeps the `AgentActor` alive** and snapshots the clean pre-run state
  (the run's partial events are discarded); the `WorkflowActor` reuses that child
  on `Resume`, or spawns a fresh one that recovers from the snapshot after a
  process restart. A non-recoverable failure discards partial progress and stops
  the child; recovery restarts from the last snapshot.
- **`RuntimeClient` is reused from the existing `runtime-client` crate** rather than
  redefined. `DefaultToolboxFactory` builds the standard runtime-backed tools and
  narrows them to each agent's `allowed_tools` via a filtering toolbox wrapper.

---

## 6. Structured output and expression-based transitions

This section supersedes the per-tool handoff/transition model sketched in §2.2–§2.5.
Agents now produce **structured output** and the graph routes on it.

### 6.1 Schema (`workflow.fl`)

```
struct WorkflowTransition {
    to:        String,
    condition: Option<String>,   // expr over `output`; None = unconditional catch-all
}

struct WorkflowAgentDef {
    name:           String,
    system_prompt:  Option<String>,
    model:          String,
    output_schema:  Option<Any>,           // JSON Schema for the agent's result
    allow_ask_user: bool,                  // may the agent pause to ask the user?
    transitions:    Option<Vec<WorkflowTransition>>,
    max_iterations: Option<u32>,
    max_retries:    Option<u32>,
    allowed_tools:  Option<Vec<String>>,
}

struct WorkflowDefinition { start: String, agents: Vec<WorkflowAgentDef> }
```

Transitions live on the producing agent and are tried in order; the first whose
`condition` evaluates to `true` wins. `condition` is an [`eval`](https://crates.io/crates/eval)
expression with the agent's output bound to `output` (e.g. `output.score > 80`).
No match ⇒ the workflow finishes with that output.

### 6.2 The `conclude` terminal tool

An agent ends its turn by calling a single builtin tool, `conclude`, synthesized
by the `workflow` crate per agent config:

| output_schema | allow_ask_user | `conclude` input schema                         |
|---------------|----------------|-------------------------------------------------|
| none          | false          | tool not registered (agent ends with plain text)|
| present       | false          | the output schema itself                        |
| none          | true           | `{ question, choices? }`                        |
| present       | true           | `kind`-tagged: `submit { output }` \| `ask { question, choices? }` |

The tool is advertised to the model but never executed — the agent loop intercepts
it. `AgentActor` reads the payload: a submit/output → `AgentConcluded { output }`
(the workflow evaluates transitions); an ask → `AgentAsked` → `AwaitingUserInput`,
resumed by injecting the user's reply as the tool result.

### 6.3 agentcore stays generic

`agentcore::Agent` takes a single optional `handoff_tool` (name only). At `build()`
it verifies the tool is advertised in the toolbox and compiles a validator from the
tool's declared `input_schema`. When the model calls it, the run ends as a `Handoff`
**only if** it is the sole tool call and its input validates; otherwise the model is
nudged via tool-result errors to re-issue it, bounded by `handoff_max_retries`.
agentcore knows nothing of `conclude`, submit/ask, or transitions — those are the
`workflow` crate's concern.

**Tool choice.** When a handoff tool is configured, every provider call is made with
Anthropic `tool_choice: any` — the model must call *some* tool each turn, so it can
keep doing work but can never end with bare text; it finishes only by calling the
handoff tool. (`tool_choice: tool=<handoff>` would force that specific tool
immediately and preclude any prior tool work, so it is not used.) Agents with no
handoff tool run with `tool_choice: auto` and may end their turn with text.

As a safety net (for providers that ignore `tool_choice`), if a handoff agent's
model ends a turn with plain text and no tool call, agentcore nudges it to call the
handoff tool and, after `handoff_max_retries`, fails with `HandoffValidationFailed`
rather than silently completing — so a handoff agent never returns unstructured text.
