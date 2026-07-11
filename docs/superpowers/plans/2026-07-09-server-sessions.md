# Server Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the `server` crate into a session-oriented web backend: launch persisted, recoverable interactive agent sessions over HTTP POST + SSE, backed by a vendor-agnostic runtime protocol.

**Architecture:** New event-sourced actors (`SessionSupervisor → SessionActor → AgentActor`) on the existing `horsie-actor` core, an axum HTTP/SSE layer, and a `RuntimeVendor` trait whose first impl wraps the existing local-process executor assembly. Lazy recovery: journals replay at startup, runtimes respawn only on user action.

**Tech Stack:** Rust (edition 2024), axum 0.7 (workspace dep), tokio, fluorite codegen for all wire types, TypeScript types via `fluorite ts`.

**Spec:** `docs/superpowers/specs/2026-07-09-server-sessions-design.md` (committed on branch `server-sessions`).

## Global Constraints

- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` (workspace lints). Test modules opt out with the standard `#[allow(...)]` / `#![cfg_attr(test, allow(...))]` blocks seen in every existing file.
- Every match on a fluorite-generated enum/union must be exhaustive with named arms (no `_`).
- Protocol types are fluorite-generated (`models/fluorite/*.fl`); persisted/storage types are hand-written Rust in the owning crate. Never conflate them.
- Unit tests live in `#[cfg(test)] mod tests` in the same file; full-stack tests in `tests/` crate.
- Pre-commit gate per task: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test -p <touched-crates>`. Final gate: `make check`.
- Commit style: short imperative subject, no body unless needed, **no AI attribution of any kind**.
- All work on branch `server-sessions`.

## File Structure

```
models/fluorite/executor.fl        MODIFY  add StopRuntime/AttachRuntime/DeleteRuntime commands
models/fluorite/session.fl         CREATE  session wire types + SSE event union
models/fluorite/session_api.fl     CREATE  HTTP request/response types + ApiError
models/src/lib.rs                  MODIFY  include new generated modules
workflow/src/context.rs            MODIFY  AgentOutcome + AgentOutcomeSink; AgentRuntimeContext.parent
workflow/src/agent_actor.rs        MODIFY  deliver outcomes via sink; `interactive` knob; sanitize on Run
workflow/src/workflow_actor.rs     MODIFY  WorkflowParent sink impl
workflow/src/lib.rs                MODIFY  export AgentOutcome, AgentOutcomeSink, SharedContext
executor/src/inmem_transport.rs    MODIFY  handle Stop/Attach/Delete commands
executor/src/executor.rs           MODIFY  exhaustive-match arms for new commands (WS executor)
executor-client/src/client.rs      MODIFY  stop_runtime / attach_runtime / delete_runtime
server/src/vendor/mod.rs           CREATE  RuntimeVendor trait, RuntimeSpec, VendorRuntime, VendorError
server/src/vendor/local.rs         CREATE  LocalProcessVendor (executor assembly per runtime)
server/src/vendor/mock.rs          CREATE  MockVendor (signal-recording, for tests)
server/src/sessions/spec.rs        CREATE  storage types: SessionSpec, AgentSettings, SessionStatus, ServerDeps
server/src/sessions/supervisor.rs  CREATE  SessionSupervisor actor (registry)
server/src/sessions/session_actor.rs CREATE SessionActor (state machine + vendor signals)
server/src/sessions/events.rs      CREATE  journal→wire event mapping, SessionFrame, SessionEventSink
server/src/sessions/mod.rs         CREATE  module wiring
server/src/http/mod.rs             CREATE  axum router + AppState
server/src/http/handlers.rs        CREATE  REST handlers
server/src/http/sse.rs             CREATE  SSE handlers (per-session + global)
server/src/http/error.rs           CREATE  ApiError mapping
server/src/lib.rs                  MODIFY  export new modules (keep existing WS server exports)
server/Cargo.toml                  MODIFY  add deps (axum, actor, workflow, executor, clients, tokio-stream…)
cli/src/main.rs                    MODIFY  `horsie serve` subcommand
cli/src/serve.rs                   CREATE  serve wiring (config → deps → supervisor → axum)
cli/src/lib.rs                     MODIFY  `pub mod serve;`
cli/Cargo.toml                     MODIFY  add horsie-server dep
clients/ts/package.json            CREATE  TS types package (fluorite ts codegen)
Makefile                           MODIFY  `ts-types` target
tests/Cargo.toml                   MODIFY  add server/actor/workflow/models/reqwest deps
tests/tests/session_server_e2e.rs  CREATE  full-stack integration tests
```

---

### Task 1: AgentOutcome + AgentOutcomeSink (decouple agent from workflow parent)

**Files:**
- Modify: `workflow/src/context.rs`
- Modify: `workflow/src/agent_actor.rs` (handle_finished, park_or_resume)
- Modify: `workflow/src/workflow_actor.rs` (WorkflowParent, spawn_agent)
- Modify: `workflow/src/lib.rs`

**Interfaces:**
- Produces (used by Tasks 2, 7):
  ```rust
  // workflow/src/context.rs
  #[derive(Debug, Clone)]
  pub enum AgentOutcome {
      Concluded { session_id: Uuid, output: serde_json::Value },
      Asked { session_id: Uuid, tool_call_id: Option<String>, question: String },
      Parked { session_id: Uuid },
      Failed { session_id: Uuid, error: String, recoverable: bool },
  }
  #[async_trait]
  pub trait AgentOutcomeSink: Send + Sync {
      async fn deliver(&self, outcome: AgentOutcome);
  }
  // AgentRuntimeContext: field `parent_ref: ActorRef<WorkflowCommand>` REPLACED by
  // `parent: Arc<dyn AgentOutcomeSink>`
  ```
- Behavior preserved: `WorkflowActor` receives exactly the same `WorkflowCommand::Agent*` variants as before, via a `WorkflowParent` adapter.

- [ ] **Step 1: Write the failing test** — in `workflow/src/workflow_actor.rs` tests module, add a test proving the adapter maps outcomes to commands. (It fails to compile until the types exist — a compile-fail is the failure signal for refactor tasks.)

```rust
#[tokio::test]
async fn workflow_parent_maps_outcomes_to_commands() {
    use crate::context::{AgentOutcome, AgentOutcomeSink};
    // A tiny actor stub is overkill: WorkflowParent wraps an ActorRef<WorkflowCommand>;
    // use a raw mpsc-backed ActorRef via a test actor. Simplest: spawn a real
    // WorkflowActor is heavy — instead assert the mapping function directly.
    let session_id = Uuid::new_v4();
    let cmd = map_outcome(AgentOutcome::Failed {
        session_id,
        error: "boom".into(),
        recoverable: true,
    });
    match cmd {
        WorkflowCommand::AgentFailed { session_id: s, error, recoverable } => {
            assert_eq!(s, session_id);
            assert_eq!(error, "boom");
            assert!(recoverable);
        }
        WorkflowCommand::Start { .. }
        | WorkflowCommand::Cancel
        | WorkflowCommand::Resume { .. }
        | WorkflowCommand::Fork { .. }
        | WorkflowCommand::AgentConcluded { .. }
        | WorkflowCommand::AgentAsked { .. }
        | WorkflowCommand::AgentParked { .. } => panic!("wrong mapping"),
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-workflow workflow_parent_maps 2>&1 | tail -5`
Expected: compile error — `AgentOutcome` not found.

- [ ] **Step 3: Implement.** In `context.rs`: add `AgentOutcome` + `AgentOutcomeSink` (exact shapes above; `use uuid::Uuid;` and `use serde_json::Value;` already imported). Change `AgentRuntimeContext`:

```rust
#[derive(Clone)]
pub struct AgentRuntimeContext {
    pub provider: Arc<dyn LlmProvider>,
    pub toolbox: Arc<dyn Toolbox>,
    pub event_sink: Arc<dyn EventSink>,
    /// Whoever spawned this agent; receives its terminal outcome.
    pub parent: Arc<dyn AgentOutcomeSink>,
    pub session_id: Uuid,
}
```

Remove the now-unused `use crate::workflow_actor::{WorkflowCommand, WorkflowNotification}` from context.rs imports if `WorkflowNotification` remains used elsewhere in the file keep it — check compiler.

In `workflow_actor.rs`: add the adapter + pure mapping fn:

```rust
/// Adapts a workflow's mailbox to the [`AgentOutcomeSink`] its child agents report to.
pub(crate) struct WorkflowParent(pub ActorRef<WorkflowCommand>);

pub(crate) fn map_outcome(outcome: crate::context::AgentOutcome) -> WorkflowCommand {
    use crate::context::AgentOutcome;
    match outcome {
        AgentOutcome::Concluded { session_id, output } => {
            WorkflowCommand::AgentConcluded { session_id, output }
        }
        AgentOutcome::Asked { session_id, tool_call_id, question } => {
            WorkflowCommand::AgentAsked { session_id, tool_call_id, question }
        }
        AgentOutcome::Parked { session_id } => WorkflowCommand::AgentParked { session_id },
        AgentOutcome::Failed { session_id, error, recoverable } => {
            WorkflowCommand::AgentFailed { session_id, error, recoverable }
        }
    }
}

#[async_trait]
impl crate::context::AgentOutcomeSink for WorkflowParent {
    async fn deliver(&self, outcome: crate::context::AgentOutcome) {
        let _ = self.0.tell(map_outcome(outcome)).await;
    }
}
```

In `spawn_agent` (workflow_actor.rs:253-259), replace `parent_ref: ctx.self_ref()` with `parent: Arc::new(WorkflowParent(ctx.self_ref()))`.

In `agent_actor.rs`, rewrite `handle_finished` and `park_or_resume` to deliver outcomes instead of telling `WorkflowCommand`s. Every `parent.tell(WorkflowCommand::AgentX {...})` becomes `parent.deliver(AgentOutcome::X {...})`. The signature of `park_or_resume` changes its `parent: ActorRef<WorkflowCommand>` param to `parent: Arc<dyn AgentOutcomeSink>`; `let parent = self.ctx.parent.clone();` at the top of `handle_finished`. Remove `use crate::workflow_actor::WorkflowCommand;` from agent_actor.rs.

In `lib.rs`, extend the context export line:

```rust
pub use context::{
    AgentOutcome, AgentOutcomeSink, AgentRuntimeContext, CONCLUDE_TOOL, DefaultToolboxFactory,
    INSPECT_WORKSPACE_TOOL, SKILL_TOOL, ToolboxFactory, WorkflowRuntimeContext, conclude_tool_spec,
};
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-workflow 2>&1 | tail -5`
Expected: all pass (existing + new).

- [ ] **Step 5: Workspace still compiles** — `cargo clippy --all-targets -- -D warnings 2>&1 | tail -5` (supervisor/cli consume workflow; no other crate constructs `AgentRuntimeContext`, so only workflow files change).

- [ ] **Step 6: Commit** — `git add -A && git commit -m "workflow: decouple agent outcome reporting via AgentOutcomeSink"`

---

### Task 2: Interactive agent mode (suppress auto-resume, no compaction, sanitized Run history)

**Files:**
- Modify: `workflow/src/agent_actor.rs`
- Modify: `workflow/src/context.rs` (no change expected; verify)
- Modify: `workflow/src/lib.rs` (export `SharedContext` from workspace)
- Modify: `workflow/src/workspace.rs` (make `SharedContext` pub if not already)

**Interfaces:**
- Produces (used by Task 7):
  - `AgentParams` gains `pub interactive: bool`; `AgentParams::from_def` sets `interactive: false`.
  - Interactive semantics: `on_recovery_complete` re-arms timers but never starts a synthetic-continue run; Ask/Cancelled/Parked effects skip `.and_snapshot()` (full event log preserved for SSE cursor stability).
  - `AgentCommand::Run` / `InjectToolResult` pass `sanitize_for_resume(state.messages.clone())` as history (all modes — a no-op on well-formed history).
  - `pub use workspace::SharedContext;` added to lib exports.

- [ ] **Step 1: Write failing tests** (agent_actor.rs tests module):

```rust
#[test]
fn from_def_defaults_to_non_interactive() {
    let def = WorkflowAgentDef {
        use_plugins: None, name: "a".into(), system_prompt: None, model: "m".into(),
        output_schema: None, allow_ask_user: false, allow_timers: None,
        transitions: None, max_iterations: None, max_retries: None, allowed_tools: None,
    };
    assert!(!AgentParams::from_def(&def).interactive);
}
```

Plus, since the recovery/snapshot behavior is exercised through `handle_finished` effects, assert the effect shape via a helper: make `handle_finished` testable by checking `CommandEffect` fields. `CommandEffect`'s fields are `pub(crate)` in horsie-actor — not inspectable here. Instead test the observable knob: extract the decision into a pure function and test that:

```rust
#[test]
fn interactive_ask_does_not_compact() {
    assert!(!compact_on_pause(true));
    assert!(compact_on_pause(false));
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p horsie-workflow interactive 2>&1 | tail -5` → compile error (`interactive` field missing).

- [ ] **Step 3: Implement.**

`AgentParams`: add field + doc:

```rust
/// Interactive (session) mode: recovery never injects a synthetic continue —
/// the next user message is the continuation — and the event log is never
/// compacted (SSE cursors are journal sequence numbers and must stay stable).
pub interactive: bool,
```

`from_def`: `interactive: false,`. Add pure helper near `AgentParams`:

```rust
/// Whether a pause (ask/park/cancel) may snapshot-compact the journal.
fn compact_on_pause(interactive: bool) -> bool {
    !interactive
}
```

Apply in `handle_finished`:
- Ask branch: `if compact_on_pause(self.params.interactive) { CommandEffect::snapshot() } else { CommandEffect::none() }`
- Cancelled branch: `let eff = CommandEffect::persist(vec![AgentDomainEvent::RunCancelled]); if compact_on_pause(self.params.interactive) { eff.and_snapshot() } else { eff }`
- `park_or_resume` Parked persist: same conditional on `.and_snapshot()`.

`on_recovery_complete`: after the timer re-arm loop, insert:

```rust
// Interactive sessions never self-continue: the user's next message is the
// continuation (the session layer passes sanitized history on Run).
if self.params.interactive {
    return;
}
```

`handle_command` Run and InjectToolResult: replace `state.messages.clone()` with `sanitize_for_resume(state.messages.clone())` in both `start_run` calls.

`workspace.rs`: ensure `pub struct SharedContext` (it is constructed in workflow_actor via `crate::workspace::SharedContext` — check its visibility; make `pub` if `pub(crate)`), and add to lib.rs:

```rust
pub use workspace::{
    SharedContext, Skill, SkillSet, WorkspaceContext, compose_system_prompt, scan as scan_workspace,
};
```

- [ ] **Step 4: Run tests** — `cargo test -p horsie-workflow 2>&1 | tail -5` → pass. Also `cargo test -p horsie-supervisor 2>&1 | tail -3` (consumer unaffected).

- [ ] **Step 5: Commit** — `git commit -am "workflow: interactive agent mode (no auto-resume, no compaction, sanitized run history)"`

---

### Task 3: Vendor wire signals — StopRuntime / AttachRuntime / DeleteRuntime

**Files:**
- Modify: `models/fluorite/executor.fl`
- Modify: `executor-client/src/client.rs`
- Modify: `executor/src/inmem_transport.rs`
- Modify: `executor/src/executor.rs` (exhaustive matches; WS executor semantics)

**Interfaces:**
- Produces (used by Task 5):
  ```rust
  impl ExecutorClient {
      pub async fn stop_runtime(&self, id: &str) -> Result<(), ClientError>;    // waits RuntimeState::Stopped
      pub async fn attach_runtime(&self, id: &str, config: RuntimeConfig) -> Result<(), ClientError>; // waits Running
      pub async fn delete_runtime(&self, id: &str) -> Result<(), ClientError>;  // waits Stopped
  }
  ```
- Wire: `ExecutorCommand` union gains `StopRuntime(StopRuntimeCmd)`, `AttachRuntime(AttachRuntimeCmd)`, `DeleteRuntime(DeleteRuntimeCmd)`.

- [ ] **Step 1: Extend the schema.** In `models/fluorite/executor.fl`, after `QueryRuntimesCmd {}`:

```
/// Halt a runtime without destroying it — the runtime stays re-attachable
/// (workspace/state preserved). The explicit signal for "user stopped the session".
struct StopRuntimeCmd { runtime_id: String }
/// Re-attach to (revive) a preserved runtime. Vendors that cannot resume in
/// place provision a fresh instance against the same config.
struct AttachRuntimeCmd { runtime_id: String, config: RuntimeConfig }
/// The owning session was deleted; the executor/vendor decides whether the
/// underlying runtime is destroyed or kept.
struct DeleteRuntimeCmd { runtime_id: String }
```

And extend the union:

```
union ExecutorCommand {
    CreateRuntime(CreateRuntimeCmd),
    DestroyRuntime(DestroyRuntimeCmd),
    RestartRuntime(RestartRuntimeCmd),
    StopRuntime(StopRuntimeCmd),
    AttachRuntime(AttachRuntimeCmd),
    DeleteRuntime(DeleteRuntimeCmd),
    QueryRuntimes(QueryRuntimesCmd),
    ToolCall(ToolCallCmd),
    CancelToolCall(CancelToolCallCmd),
}
```

- [ ] **Step 2: Build to find every non-exhaustive match** — `cargo build --workspace 2>&1 | grep -E "error|match" | head -20`. Expected: errors in `executor/src/inmem_transport.rs` and `executor/src/executor.rs` (wildcard arms are denied, so listed-arm matches break).

- [ ] **Step 3: Write failing client test** (executor-client has no direct test harness for this; the behavior test lives in inmem_transport usage — add to `executor/src/inmem_transport.rs` tests):

```rust
#[tokio::test]
async fn stop_attach_delete_signals_round_trip() {
    // NullProvider creates handles that report healthy and stop cleanly.
    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let provider: Arc<dyn RuntimeProvider> = Arc::new(InstantProvider);
    let t = InMemExecutorTransport::new(provider, connected);
    let client = horsie_executor_client::ExecutorClient::new(t);
    let cfg = RuntimeConfig { workspaces: vec![], plugins_dir: None, hook_path: vec![], env: vec![] };
    client.create_runtime("r1", cfg.clone()).await.unwrap();
    client.stop_runtime("r1").await.unwrap();
    // After stop-preserve, attach revives under the same id.
    client.attach_runtime("r1", cfg).await.unwrap();
    client.delete_runtime("r1").await.unwrap();
}
```

with a local `InstantProvider` test double (same file):

```rust
struct InstantHandle;
#[async_trait]
impl crate::provider::RuntimeHandle for InstantHandle {
    async fn stop(&self) -> Result<(), crate::error::RuntimeError> { Ok(()) }
    async fn health_check(&self) -> Result<crate::provider::HealthStatus, crate::error::RuntimeError> {
        Ok(crate::provider::HealthStatus::Healthy)
    }
}
struct InstantProvider;
#[async_trait]
impl RuntimeProvider for InstantProvider {
    async fn create(&self, _id: &str, _c: &RuntimeConfig)
        -> Result<Arc<dyn crate::provider::RuntimeHandle>, crate::error::RuntimeError> {
        Ok(Arc::new(InstantHandle))
    }
}
```

Note: `create_core` registers in the transport's internal `RuntimeRegistry`; `attach_runtime` after `stop_runtime` must not fail with `AlreadyExists` — see semantics below.

- [ ] **Step 4: Implement.**

`executor-client/src/client.rs` — three methods mirroring `create_runtime`/`destroy_runtime` exactly (fresh request UUID, loop on events):
- `stop_runtime`: send `ExecutorCommand::StopRuntime(StopRuntimeCmd { runtime_id })`, success on `RuntimeStateChanged` with `RuntimeState::Stopped`.
- `attach_runtime`: send `AttachRuntime(AttachRuntimeCmd { runtime_id, config })`, success on `Running`.
- `delete_runtime`: send `DeleteRuntime(DeleteRuntimeCmd { runtime_id })`, success on `Stopped`.
Import the three new Cmd structs.

`executor/src/inmem_transport.rs` — extend the match:
- `StopRuntime(c)`: `begin_stop` → `handle.stop()` → `complete_stop` (registry entry removed; the *preservation* is on-disk state owned by the caller, not the in-mem registry) → emit `Stopped`. On `RuntimeError` emit `CommandFailed`.
- `AttachRuntime(c)`: identical body to `CreateRuntime` (`create_core(...)`) — for a local process, attach is a respawn against preserved on-disk state; emit `Running`/`CommandFailed`.
- `DeleteRuntime(c)`: identical body to `DestroyRuntime`; emit `Stopped`/`CommandFailed`. (Distinct wire signal; local vendor's delete-vs-stop distinction is enacted by the vendor layer, Task 5.)
- Keep `RestartRuntime | QueryRuntimes | ToolCall | CancelToolCall` in the unsupported arm.

`executor/src/executor.rs` — find the `ExecutorCommand` match (WS executor). Add arms with the same semantics (`StopRuntime` → stop path, `AttachRuntime` → create path, `DeleteRuntime` → destroy path), reusing whatever helper the existing Create/Destroy arms call. Read the surrounding code first and mirror it exactly.

- [ ] **Step 5: Run** — `cargo test -p horsie-executor -p horsie-executor-client 2>&1 | tail -5` → pass; `cargo build --workspace 2>&1 | tail -3` → clean.

- [ ] **Step 6: Commit** — `git commit -am "executor: stop/attach/delete runtime signals across the vendor wire protocol"`

---

### Task 4: Session wire schemas (`session.fl`, `session_api.fl`)

**Files:**
- Create: `models/fluorite/session.fl`
- Create: `models/fluorite/session_api.fl`
- Modify: `models/src/lib.rs`

**Interfaces:**
- Produces Rust types under `horsie_models::session::*` and `horsie_models::session_api::*` (used by Tasks 8, 9, 11). Names below are authoritative.

- [ ] **Step 1: Write `models/fluorite/session.fl`:**

```
/// Wire types for the session server: session views and the SSE event union.
package session;

use agent.Message;

/// User-visible lifecycle state of a session. Failure reasons ride separately
/// in `last_error` so the enum stays a plain discriminant.
enum SessionStatusKind {
    Provisioning,
    Idle,
    Running,
    AwaitingInput,
    Interrupted,
    Stopped,
    RecoveryFailed,
    Failed,
}

/// Agent settings supplied at session creation.
struct AgentSettings {
    model: String,
    system_prompt: Option<String>,
    allowed_tools: Option<Vec<String>>,
    allow_ask_user: Option<bool>,
    use_plugins: Option<bool>,
    max_iterations: Option<u32>,
    max_retries: Option<u32>,
}

struct SessionSummary {
    id: String,
    name: Option<String>,
    status: SessionStatusKind,
    created_at: u64,
    last_error: Option<String>,
}

struct SessionDetail {
    id: String,
    name: Option<String>,
    status: SessionStatusKind,
    created_at: u64,
    last_error: Option<String>,
    /// The question the agent is awaiting an answer to (status AwaitingInput).
    pending_question: Option<String>,
    model: String,
    workdirs: Vec<String>,
    vendor: String,
}

// --- SSE event payloads ---

/// A complete transcript message (user, assistant, or tool result), replayed
/// from the durable journal. Carries an SSE id (the journal sequence number).
struct MessageEvent { message: Message }
struct ToolResultEvent { tool_call_id: String, output: String, is_error: bool }
struct TurnCompletedEvent { iterations: u32, input_tokens: u64, output_tokens: u64 }
struct AskedEvent { question: String }
/// Live status transition. Sent without an SSE id (the session detail endpoint
/// is the durable source for status).
struct StatusChangedEvent { status: SessionStatusKind, reason: Option<String> }
struct ErrorEvent { message: String }
/// Streaming text delta — live only, never journaled, never carries an SSE id.
struct DeltaEvent { text: String }
struct ToolStartEvent { tool_call_id: String, name: String }

#[type_tag = "type"]
union SessionEvent {
    Message(MessageEvent),
    ToolResult(ToolResultEvent),
    TurnCompleted(TurnCompletedEvent),
    Asked(AskedEvent),
    StatusChanged(StatusChangedEvent),
    Error(ErrorEvent),
    Delta(DeltaEvent),
    ToolStart(ToolStartEvent),
}

/// One frame on the global `/api/events` stream (live session list updates).
struct GlobalSessionEvent {
    session_id: String,
    status: SessionStatusKind,
    reason: Option<String>,
}
```

- [ ] **Step 2: Write `models/fluorite/session_api.fl`:**

```
/// HTTP request/response contracts for the session server.
package session_api;

use session.AgentSettings;
use session.SessionSummary;
use session.SessionDetail;
use capabilities.CapabilitySpec;

struct CreateSessionRequest {
    name: Option<String>,
    agent: AgentSettings,
    /// Workspace roots (>=1), like `horsie job run --workdir`.
    workdirs: Vec<String>,
    /// Runtime vendor name; defaults to "local".
    vendor: Option<String>,
    /// Capability spec overriding the server default.
    capabilities: Option<CapabilitySpec>,
}

struct CreateSessionResponse { session: SessionSummary }
struct ListSessionsResponse { sessions: Vec<SessionSummary> }
struct GetSessionResponse { session: SessionDetail }
struct SendMessageRequest { text: String }
struct AckResponse {}

/// Uniform HTTP error envelope.
struct ApiError { code: String, message: String }
```

- [ ] **Step 3: Wire into `models/src/lib.rs`** (after the `workflow` module block, same pattern):

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod session {
    include!(concat!(env!("OUT_DIR"), "/session/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod session_api {
    include!(concat!(env!("OUT_DIR"), "/session_api/mod.rs"));
}
```

- [ ] **Step 4: Build + smoke test** — `cargo build -p horsie-models 2>&1 | tail -3` → clean. Add one round-trip test in models/src/lib.rs tests (find the existing `#[cfg(test)]` module or create one at the bottom following repo style):

```rust
#[test]
fn session_event_round_trips_with_type_tag() {
    let ev = session::SessionEvent::Delta(session::DeltaEvent { text: "hi".into() });
    let json = serde_json::to_string(&ev).unwrap();
    assert!(json.contains("\"type\""));
    let back: session::SessionEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(ev, back);
}
```

Run: `cargo test -p horsie-models 2>&1 | tail -3` → pass.

- [ ] **Step 5: Commit** — `git commit -am "models: session + session_api wire schemas"`

---

### Task 5: Vendor layer — RuntimeVendor trait, LocalProcessVendor, MockVendor

**Files:**
- Create: `server/src/vendor/mod.rs`, `server/src/vendor/local.rs`, `server/src/vendor/mock.rs`
- Modify: `server/src/lib.rs`, `server/Cargo.toml`

**Interfaces:**
- Produces (used by Tasks 6, 7, 10, 12):

```rust
// server/src/vendor/mod.rs
pub struct RuntimeSpec {
    pub workspaces: Vec<horsie_models::Workspace>,
    pub capabilities_file: PathBuf,   // written by the session layer before any vendor call
    pub plugins_dir: Option<PathBuf>,
    pub hook_path: Vec<PathBuf>,
}
pub struct VendorRuntime {
    pub runtime_client: RuntimeClient,
    pub handle: Arc<dyn VendorRuntimeHandle>,
}
#[async_trait]
pub trait VendorRuntimeHandle: Send + Sync {
    /// Halt without destroying (stop-preserve). Idempotent.
    async fn stop(&self);
}
#[async_trait]
pub trait RuntimeVendor: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    /// Provision a brand-new runtime.
    async fn create(&self, runtime_id: &str, spec: &RuntimeSpec) -> Result<VendorRuntime, VendorError>;
    /// Revive a preserved runtime (respawn / resume / restart as the vendor sees fit).
    async fn attach(&self, runtime_id: &str, spec: &RuntimeSpec) -> Result<VendorRuntime, VendorError>;
    /// The owning session was deleted; the vendor decides the runtime's fate.
    /// Callable with no live handle (e.g. after a restart).
    async fn delete(&self, runtime_id: &str);
}
#[derive(Debug, thiserror::Error)]
pub enum VendorError {
    #[error("provision failed: {0}")] Provision(String),
    #[error("attach failed: {0}")] Attach(String),
}
```

- `MockVendor` (pub, `server::vendor::mock::MockVendor`): records every signal as strings (`"create:<id>"`, `"attach:<id>"`, `"stop:<id>"`, `"delete:<id>"`) into an `Arc<Mutex<Vec<String>>>` accessor `signals()`; constructor knobs `fail_attach_times(n)`, `fail_create(bool)`; returns `VendorRuntime` whose `RuntimeClient` is `RuntimeClient::new(MockTransport::ok(""))`.

- [ ] **Step 1: server/Cargo.toml deps.** Add:

```toml
horsie-actor          = { path = "../actor", features = ["file-journal"] }
horsie-workflow       = { path = "../workflow" }
horsie-agentcore      = { path = "../agentcore" }
horsie-executor       = { path = "../executor" }
horsie-executor-client = { path = "../executor-client" }
horsie-runtime-client = { path = "../runtime-client" }
axum              = { workspace = true }
tokio-stream      = { workspace = true }
tokio-util        = { workspace = true }
serde             = { workspace = true }
tracing           = { workspace = true }
```

(Check `actor/Cargo.toml` for the exact file-journal feature name first; if `FileJournal` is behind `feature = "file-journal"` and the daemon's cli dep enables it, mirror the cli's dependency line.)

- [ ] **Step 2: Failing test for MockVendor semantics** (in `server/src/vendor/mock.rs` tests):

```rust
#[tokio::test]
async fn mock_vendor_records_signals_and_fails_attach_on_demand() {
    let v = MockVendor::new().fail_attach_times(1);
    let spec = test_spec();
    assert!(v.create("s1", &spec).await.is_ok());
    assert!(v.attach("s1", &spec).await.is_err()); // first attach fails
    assert!(v.attach("s1", &spec).await.is_ok());  // then succeeds
    v.delete("s1").await;
    assert_eq!(v.signals(), vec!["create:s1", "attach:s1", "attach:s1", "delete:s1"]);
}
```

`test_spec()` builds a `RuntimeSpec` with empty workspaces and a tmp caps path.

- [ ] **Step 3: Run to verify failure** — `cargo test -p horsie-server mock_vendor 2>&1 | tail -5` → compile error.

- [ ] **Step 4: Implement `mod.rs` (types above), `mock.rs`:**

```rust
pub struct MockVendor {
    signals: Arc<Mutex<Vec<String>>>,
    fail_attach: Arc<Mutex<u32>>,
    fail_create: bool,
}
```

`create`: push `create:<id>`; if `fail_create` → `Err(VendorError::Provision("mock create failure".into()))`; else `Ok(VendorRuntime { runtime_client: RuntimeClient::new(MockTransport::ok("")), handle: Arc::new(MockHandle { signals, id }) })` where `MockHandle::stop` pushes `stop:<id>`. `attach`: push `attach:<id>`; decrement fail counter, `Err(VendorError::Attach(...))` while > 0. `delete`: push `delete:<id>`.

Implement `local.rs` — `LocalProcessVendor`:

```rust
/// Runtime vendor backed by a nono-sandboxed `horsie-runtime` child process.
/// Each create/attach builds a fresh executor assembly (listener + connected
/// registry + provider + in-mem transport) exactly like the daemon's
/// ProcessJobRuntime; stop kills the child but preserves all on-disk state
/// (workspace + capability file), so attach can respawn against it.
pub struct LocalProcessVendor {
    runtime_bin: PathBuf,
}
impl LocalProcessVendor {
    pub fn new(runtime_bin: PathBuf) -> Self { Self { runtime_bin } }
}
```

`create(runtime_id, spec)` body (adapted verbatim from `supervisor/src/job_actor.rs:117-195`, minus hackamore):
1. `let connected = Arc::new(ConnectedRuntimeRegistry::new());`
2. socket path helper (copy `socket_path()` from job_actor.rs:90-104 into this file — it is private to supervisor),
3. `RuntimeListenerServer::bind(RuntimeEndpoint::Unix(sock.clone()))` + `CancellationToken` + `serve_runtime_connections(...)`,
4. `ProcessRuntimeProvider::new(self.runtime_bin.clone(), RuntimeEndpoint::Unix(sock), connected.clone()).with_sandbox(SandboxPolicy { capabilities_file: spec.capabilities_file.clone() })`,
5. `let client = ExecutorClient::new(InMemExecutorTransport::new(Arc::new(provider), connected));`
6. `client.create_runtime(runtime_id, runtime_config_from(spec)).await.map_err(|e| VendorError::Provision(e.to_string()))?;`
7. `let transport = client.runtime_transport(runtime_id).await.map_err(|e| VendorError::Provision(e.to_string()))?;`
8. `Ok(VendorRuntime { runtime_client: RuntimeClient::from_arc(transport), handle: Arc::new(LocalHandle { client, cancel, runtime_id: runtime_id.to_string() }) })`

`attach` = the same assembly but step 6 calls `client.attach_runtime(...)` (the distinct wire signal), error → `VendorError::Attach`.

```rust
fn runtime_config_from(spec: &RuntimeSpec) -> RuntimeConfig {
    RuntimeConfig {
        workspaces: spec.workspaces.iter().map(|w| WorkspaceConfig {
            name: w.name.clone(),
            path: w.path.to_string_lossy().into_owned(),
        }).collect(),
        plugins_dir: spec.plugins_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
        hook_path: spec.hook_path.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        env: vec![],
    }
}

struct LocalHandle { client: ExecutorClient, cancel: CancellationToken, runtime_id: String }
#[async_trait]
impl VendorRuntimeHandle for LocalHandle {
    async fn stop(&self) {
        let _ = self.client.stop_runtime(&self.runtime_id).await;
        self.cancel.cancel();
    }
}
```

`RuntimeVendor::delete` for local: nothing to reclaim beyond what stop released — the user's workspace is never touched; per-session server state dirs are owned by the session layer. Log at debug and return. `fn name(&self) -> &'static str { "local" }`.

`server/src/lib.rs`: add `pub mod vendor;` (keep existing exports).

- [ ] **Step 5: Run** — `cargo test -p horsie-server 2>&1 | tail -5` → pass (mock tests; local vendor is exercised in Task 12/manually since it needs the runtime binary).

- [ ] **Step 6: Commit** — `git commit -am "server: RuntimeVendor protocol layer with local process vendor"`

---

### Task 6: Session storage types + SessionSupervisor actor

**Files:**
- Create: `server/src/sessions/spec.rs`, `server/src/sessions/supervisor.rs`, `server/src/sessions/mod.rs`
- Modify: `server/src/lib.rs`

**Interfaces:**
- Produces (used by Tasks 7-10, 12):

```rust
// spec.rs — STORAGE types (journal-owned), never wire types.
pub type SessionId = String;   // uuid string; equals the agent session uuid

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    pub model: String,
    pub system_prompt: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub allow_ask_user: bool,
    pub use_plugins: Option<bool>,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub name: Option<String>,
    pub agent: AgentSettings,
    pub workspaces: Vec<horsie_models::Workspace>,
    pub capabilities: horsie_models::capabilities::CapabilitySpec, // resolved at create
    pub vendor: String,
    pub plugins_dir: Option<PathBuf>,
    pub hook_path: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Provisioning, Idle, Running, AwaitingInput, Interrupted, Stopped,
    RecoveryFailed { reason: String },
    Failed { reason: String },
}

/// Process-wide deps injected into every SessionActor.
#[derive(Clone)]
pub struct ServerDeps {
    pub provider_registry: HashMap<String, Arc<dyn LlmProvider>>,
    pub vendors: HashMap<String, Arc<dyn RuntimeVendor>>,
    /// Per-session server state (capability files) under `<state_dir>/sessions/<id>/`.
    pub state_dir: PathBuf,
}

// supervisor.rs
pub enum SessionSupervisorCommand {
    Create { spec: SessionSpec, created_at: u64, reply: oneshot::Sender<SessionId> },
    List { reply: oneshot::Sender<Vec<(SessionId, SessionRecord)>> },
    Get { id: SessionId, reply: oneshot::Sender<Option<SessionRecord>> },
    UserMessage { id: SessionId, text: String, reply: oneshot::Sender<Result<(), UserMessageError>> },
    Stop { id: SessionId, reply: oneshot::Sender<Result<(), String>> },
    Delete { id: SessionId, reply: oneshot::Sender<Result<(), String>> },
    Subscribe { id: SessionId, reply: oneshot::Sender<Option<broadcast::Receiver<SessionFrame>>> },
    Shutdown { reply: oneshot::Sender<()> },
    SessionStatusChanged { id: SessionId, status: SessionStatus }, // from children
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionSupervisorEvent {
    SessionCreated { id: SessionId, spec: SessionSpec, created_at: u64 },
    SessionStatusChanged { id: SessionId, status: SessionStatus },
    SessionDeleted { id: SessionId },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord { pub spec: SessionSpec, pub status: SessionStatus, pub created_at: u64 }
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSupervisorState { pub sessions: BTreeMap<SessionId, SessionRecord> }

pub struct SessionSupervisor { /* deps, global_tx, children: BTreeMap<SessionId, ActorRef<SessionCommand>> */ }
impl SessionSupervisor {
    pub fn new(deps: ServerDeps, global_tx: broadcast::Sender<GlobalSessionEvent>) -> Self;
}
// persistence id: ("session-supervisor", "main")
```

`GlobalSessionEvent` here is the wire type `horsie_models::session::GlobalSessionEvent` (acceptable on a broadcast channel — it is a protocol type transported to SSE clients). `SessionFrame` and `UserMessageError` come from Task 7/8 — to keep this task self-contained, define both **now** in `sessions/mod.rs` as:

```rust
// sessions/mod.rs
pub mod spec;
pub mod supervisor;
pub mod session_actor;   // Task 7 (stub module now, filled next task)
pub mod events;          // Task 8 (stub)

/// Live broadcast frames for one session's SSE stream.
#[derive(Debug, Clone)]
pub enum SessionFrame {
    /// Streaming text delta (id-less).
    Delta { text: String },
    /// A tool call started (id-less).
    ToolStart { tool_call_id: String, name: String },
    /// One or more coarse events were journaled — SSE handlers replay the
    /// journal after their cursor to pick them up with stable ids.
    Journaled,
    /// Live status transition (id-less; durable status lives in the registry).
    Status { status: spec::SessionStatus },
}

#[derive(Debug, thiserror::Error)]
pub enum UserMessageError {
    #[error("session not found")] NotFound,
    #[error("session is provisioning")] Provisioning,
    #[error("a turn is already in flight")] TurnInFlight,
    #[error("runtime recovery failed: {0}")] RecoveryFailed(String),
}
```

For this task, `session_actor.rs` is created as a minimal compiling skeleton (the full state machine is Task 7): `SessionCommand` enum variants `Provision`, `UserMessage`, `Stop`, `Delete`, `Subscribe`, `Shutdown`, `AgentOutcome(AgentOutcome)`, `ReconcileInterrupted` with the exact shapes Task 7 specifies, plus a `SessionActor::new(id: Uuid, spec: SessionSpec, deps: ServerDeps, parent: ActorRef<SessionSupervisorCommand>) -> Self` whose `handle_command` replies with errors/no-ops. This lets the supervisor compile and be fully tested now.

- [ ] **Step 1: Write failing tests** (supervisor.rs tests — pure fold tests, mirroring `supervisor_actor.rs` tests):

```rust
#[test]
fn created_then_status_then_deleted_folds() {
    let s = SessionSupervisor::apply_event(SessionSupervisorState::default(),
        SessionSupervisorEvent::SessionCreated { id: "s1".into(), spec: spec_fixture(), created_at: 7 });
    assert_eq!(s.sessions.get("s1").unwrap().status, SessionStatus::Provisioning);
    let s = SessionSupervisor::apply_event(s,
        SessionSupervisorEvent::SessionStatusChanged { id: "s1".into(), status: SessionStatus::Idle });
    assert_eq!(s.sessions.get("s1").unwrap().status, SessionStatus::Idle);
    let s = SessionSupervisor::apply_event(s, SessionSupervisorEvent::SessionDeleted { id: "s1".into() });
    assert!(s.sessions.is_empty());
}

#[tokio::test]
async fn create_list_get_round_trip() {
    let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
    let (gtx, _grx) = broadcast::channel(16);
    let sup = spawn_root(SessionSupervisor::new(test_deps(), gtx), journal);
    let id = sup.ask(|reply| SessionSupervisorCommand::Create {
        spec: spec_fixture(), created_at: 1, reply }).await.unwrap();
    let list = sup.ask(|reply| SessionSupervisorCommand::List { reply }).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, id);
    let rec = sup.ask(|reply| SessionSupervisorCommand::Get { id: id.clone(), reply }).await.unwrap();
    assert!(rec.is_some());
}
```

`spec_fixture()`: minimal SessionSpec (model "mock", vendor "mock", one tmp workspace, `CapabilitySpec { network: NetworkPolicy::Block(BlockNetwork {}), grants: vec![], unsafe_seatbelt_rules: None }`). `test_deps()`: empty provider registry, `vendors: {"mock": Arc::new(MockVendor::new())}`, tempdir state_dir (hold the `TempDir` in the test).

- [ ] **Step 2: Run to verify failure** — `cargo test -p horsie-server supervisor 2>&1 | tail -5` → compile error.

- [ ] **Step 3: Implement.** `SessionSupervisor` mirrors `SupervisorActor` (supervisor/src/supervisor_actor.rs:115-309) exactly, with these behaviors:
- `Create`: `let id = uuid::Uuid::new_v4();` → `spawn_session(ctx, id, spec.clone())` → child `tell(SessionCommand::Provision)` → reply id string → persist `SessionCreated` → also `let _ = self.global_tx.send(GlobalSessionEvent { session_id, status: SessionStatusKind::Provisioning, reason: None });`
- `apply_event`: Created inserts `SessionRecord { spec, status: SessionStatus::Provisioning, created_at }`; StatusChanged updates; Deleted removes.
- `UserMessage`/`Stop`/`Subscribe`: route to child (`self.children.get(&id)`); missing child → reply `Err(UserMessageError::NotFound)` / `Err("no such session")` / `None`. For `UserMessage`, forward the caller's oneshot into the child command (child replies directly). **If `child.tell(...)` returns Err (mailbox closed — the child's own journal recovery failed and the actor shut down), reply `Err(UserMessageError::RecoveryFailed("session unavailable: journal recovery failed".into()))` and persist a `SessionStatusChanged { status: RecoveryFailed { reason } }` so the corrupt session is visible in the list but never takes the server down (spec: recovery-failure isolation).**
- `Delete`: if child exists, `child.ask(|reply| SessionCommand::Delete { reply })`-style tell + await ack via oneshot, remove child, then persist `SessionDeleted` and send a global frame with a `Deleted`-less kind — there is no Deleted kind; send nothing on delete (list refresh covers it) — reply Ok. Unknown id → `Err("no such session")`.
- `SessionStatusChanged`: persist + `global_tx.send(...)` mapping `SessionStatus → SessionStatusKind` via a `pub fn status_kind(s: &SessionStatus) -> SessionStatusKind` helper in spec.rs (exhaustive match; RecoveryFailed/Failed map kinds, reasons ride in the global frame's `reason`).
- `Shutdown`: mirror SupervisorActor::Shutdown (fan out `SessionCommand::Shutdown`, await acks, clear children, reply).
- `on_recovery_complete`: spawn a `SessionActor` for **every** session in the recovered registry (deleted ones are gone from state), passing `Uuid::parse_str(id)` — on parse failure log error and skip. **No vendor calls, no status writes here** (children reconcile themselves — Task 7).
- persistence id `("session-supervisor", "main")`.

`spawn_session` mirrors `spawn_job`, constructing `SessionActor::new(uuid, spec, self.deps.clone(), ctx.self_ref())`.

`server/src/lib.rs`: add `pub mod sessions;`.

- [ ] **Step 4: Run** — `cargo test -p horsie-server 2>&1 | tail -5` → pass.

- [ ] **Step 5: Commit** — `git commit -am "server: session storage types and SessionSupervisor registry actor"`

---

### Task 7: SessionActor — state machine, vendor signals, agent hosting

**Files:**
- Modify: `server/src/sessions/session_actor.rs` (replace skeleton)
- Modify: `server/src/sessions/events.rs` (add `SessionEventSink` — minimal now, replay in Task 8)

**Interfaces:**
- Consumes: `AgentActor/AgentParams/AgentCommand/AgentOutcome/AgentOutcomeSink` (Tasks 1-2), `RuntimeVendor` (Task 5), supervisor command/report types (Task 6).
- Produces: the full `SessionCommand` behavior used by the supervisor routing (Task 6) and HTTP (Task 9).

```rust
pub enum SessionCommand {
    Provision,
    UserMessage { text: String, reply: oneshot::Sender<Result<(), UserMessageError>> },
    Stop { reply: oneshot::Sender<()> },
    Delete { reply: oneshot::Sender<()> },
    Subscribe { reply: oneshot::Sender<broadcast::Receiver<SessionFrame>> },
    Shutdown { reply: oneshot::Sender<()> },
    AgentOutcome(horsie_workflow::AgentOutcome),
    /// Internal: post-recovery reconciliation (Running → Interrupted).
    ReconcileInterrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionDomainEvent {
    Provisioned,
    ProvisionFailed { error: String },
    TurnStarted,
    TurnCompleted,
    TurnFailed { error: String },
    Asked { tool_call_id: Option<String>, question: String },
    Interrupted,
    AttachFailed { error: String },
    Stopped,
    Deleted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub status: Option<SessionStatus>,
    pub pending_ask: Option<String>,        // tool_call_id awaiting the user's answer
    pub pending_question: Option<String>,
    pub last_error: Option<String>,
}
```

**State machine (apply_event, exhaustive):** Provisioned→Idle; ProvisionFailed{e}→Failed{reason:e}+last_error; TurnStarted→Running (clear pending_ask/question); TurnCompleted→Idle; TurnFailed{e}→Idle+last_error; Asked{tc,q}→AwaitingInput+pending_ask/question; Interrupted→Interrupted; AttachFailed{e}→RecoveryFailed{reason:e}+last_error; Stopped→Stopped; Deleted→(keep status; actor stops via effect).

**Command handling (the vendor-signal contract):**

| Command | Precondition | Action | Effect |
|---|---|---|---|
| Provision | any | write caps file; `vendor.create(id, rt_spec)` | ok → persist Provisioned, report Idle; err → persist ProvisionFailed, report Failed |
| UserMessage | status None/Provisioning **and no live runtime** | treat as make-it-run via `create` (stale provisioning after restart) | as Provision + then run turn |
| UserMessage | Running | — | reply Err(TurnInFlight), none() |
| UserMessage | AwaitingInput + pending_ask | ensure runtime (attach if none) + ensure agent → `InjectToolResult{tc, text}` | reply Ok, none() — status flips on the agent's next outcome (idempotent-resume, same as workflow) |
| UserMessage | Idle/Stopped/Interrupted/RecoveryFailed | ensure runtime (attach; `Failed`→create) + ensure agent → `Run{text}` | reply Ok, persist TurnStarted, report Running |
| UserMessage | ensure-runtime error | — | reply Err(RecoveryFailed(e)), persist AttachFailed (or ProvisionFailed on the create path), report RecoveryFailed/Failed |
| Stop | status already Stopped | — | reply, none() |
| Stop | any other | agent.tell(Cancel); handle.stop(); clear agent+runtime | reply, persist Stopped, report Stopped |
| Delete | any | agent.tell(Cancel); handle.stop() if live; `vendor.delete(id)` | reply, persist Deleted **and_stop** |
| Shutdown | any | agent Cancel; handle.stop() if live; clear | reply, stop() — **no status persisted** so Running reconciles to Interrupted next start |
| Subscribe | any | reply `self.frames.subscribe()` | none() |
| AgentOutcome::Concluded | — | clear self.agent (it stopped) | persist TurnCompleted, report Idle |
| AgentOutcome::Asked | — | keep agent | persist Asked, report AwaitingInput |
| AgentOutcome::Failed | — | clear agent; broadcast `SessionFrame::Status` + an Error frame | persist TurnFailed, report Idle |
| AgentOutcome::Parked | — | sessions run with timers off | persist TurnFailed{"agent parked; timers are not supported in sessions"}, report Idle |
| ReconcileInterrupted | status == Some(Running) | — | persist Interrupted, report Interrupted |
| ReconcileInterrupted | anything else | — | none() |

`on_recovery_complete`: `if state.status == Some(SessionStatus::Running) { let _ = ctx.self_ref().tell(SessionCommand::ReconcileInterrupted).await; }` — nothing else (lazy recovery: no vendor calls, no agent spawn).

**Key implementation details** (write these, they are the subtle parts):

- Fields: `id: Uuid, spec: SessionSpec, deps: ServerDeps, parent: ActorRef<SessionSupervisorCommand>, frames: broadcast::Sender<SessionFrame>, runtime: Option<VendorRuntime>, agent: Option<ActorRef<AgentCommand>>`. `frames` created in `new()` with capacity 256.
- `persistence_id: ("session", <uuid string>)`. Agent child journal: `("agent", <same uuid>)` via `AgentActor::persistence_id_for(self.id)` — SessionId == agent session id by construction.
- `report(status)`: tell parent `SessionStatusChanged` + `let _ = self.frames.send(SessionFrame::Status { status });` (before returning the effect, like JobActor::report).
- caps file: `let dir = deps.state_dir.join("sessions").join(self.id.to_string()); std::fs::create_dir_all(&dir); std::fs::write(dir.join("capabilities.json"), serde_json::to_vec_pretty(&self.spec.capabilities)?)` → `RuntimeSpec { workspaces, capabilities_file, plugins_dir, hook_path }`.
- `vendor(&self) -> Result<Arc<dyn RuntimeVendor>, String>`: `self.deps.vendors.get(&self.spec.vendor).cloned().ok_or_else(|| format!("unknown runtime vendor '{}'", self.spec.vendor))`.
- `ensure_runtime(attach: bool)`: if `self.runtime.is_some()` → Ok(()); else write caps file, call `vendor.attach(...)` or `vendor.create(...)`, store. Which one: `attach` for Idle/Stopped/Interrupted/RecoveryFailed/AwaitingInput wake; `create` for None/Provisioning/Failed.
- `ensure_agent(ctx)`: if none, build:
  - provider: `self.deps.provider_registry.get(&self.spec.agent.model)` → err string if missing (persist TurnFailed + reply RecoveryFailed? No — reply `Err(UserMessageError::RecoveryFailed(msg))` and persist TurnFailed so last_error surfaces).
  - Build a `WorkflowAgentDef` carrier from `AgentSettings` (all fields mapped; `output_schema: None`, `allow_timers: None`, `transitions: None`, `name: "agent".into()`).
  - `let (ws, shared_skills) = horsie_workflow::scan_workspace(&runtime_client, None, use_plugins).await;` and bootstrap via `runtime_client.run_session_start()` when `use_plugins` (copy the `spawn_agent` block from workflow_actor.rs:231-246, adjusting paths to public exports).
  - toolbox: `DefaultToolboxFactory.for_agent(&def, runtime_client.clone(), ws.names(), use_plugins)`.
  - `let mut params = AgentParams::from_def(&def); params.interactive = true; params.system_prompt = horsie_workflow::compose_system_prompt(def.system_prompt.as_deref(), &ws, shared.as_ref());`
  - `AgentRuntimeContext { provider, toolbox, event_sink: Arc::new(SessionEventSink { frames: self.frames.clone() }), parent: Arc::new(SessionParent(ctx.self_ref())), session_id: self.id }`
  - `self.agent = Some(ctx.spawn(AgentActor::new(agent_ctx, params)));`
- `SessionParent` adapter (this file):

```rust
struct SessionParent(ActorRef<SessionCommand>);
#[async_trait]
impl AgentOutcomeSink for SessionParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        let _ = self.0.tell(SessionCommand::AgentOutcome(outcome)).await;
    }
}
```

- `SessionEventSink` (events.rs, minimal version for this task):

```rust
/// Forwards live agent events into the session's broadcast: deltas pass through
/// id-less; journaled coarse events become `Journaled` wakeups (SSE handlers
/// re-read the journal for stable ids). Best-effort — never aborts the run.
pub struct SessionEventSink { pub frames: broadcast::Sender<SessionFrame> }
#[async_trait]
impl EventSink for SessionEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        let frame = match &event {
            AgentEvent::TextChunk(e) => Some(SessionFrame::Delta { text: e.text.clone() }),
            AgentEvent::ToolCallStart(e) => Some(SessionFrame::ToolStart {
                tool_call_id: e.tool_call_id.clone(), name: e.name.clone() }),
            AgentEvent::InputMessage(_)
            | AgentEvent::MessageComplete(_)
            | AgentEvent::ToolComplete(_)
            | AgentEvent::RunComplete(_) => Some(SessionFrame::Journaled),
            AgentEvent::MessageStart(_) | AgentEvent::MessageStop(_)
            | AgentEvent::TextBlockStart(_) | AgentEvent::ThinkingBlockStart(_)
            | AgentEvent::ThinkingChunk(_) | AgentEvent::ThinkingSignatureChunk(_)
            | AgentEvent::ToolCallInputDelta(_) | AgentEvent::ContentBlockStop(_)
            | AgentEvent::ToolExecuting(_) => None,
        };
        if let Some(f) = frame { let _ = self.frames.send(f); }
        Ok(())
    }
}
```

(Ordering note for the doc comment: `PersistSink` persists each coarse event *before* forwarding to this sink, so a `Journaled` wakeup always finds the event already durable.)

- [ ] **Step 1: Write failing unit tests** (session_actor.rs tests; use `MockVendor`, `InMemoryJournal`, a provider registry with a mock LLM? No — unit tests here avoid running turns; they test transitions + signals without an LLM by never sending UserMessage on a provisioned session with a real provider. For turn-level behavior use Task 12's integration tests.)

```rust
#[test]
fn fold_covers_all_transitions() {
    use SessionDomainEvent as E;
    let s = SessionActor::apply_event(SessionState::default(), E::Provisioned);
    assert_eq!(s.status, Some(SessionStatus::Idle));
    let s = SessionActor::apply_event(s, E::TurnStarted);
    assert_eq!(s.status, Some(SessionStatus::Running));
    let s = SessionActor::apply_event(s, E::Asked { tool_call_id: Some("tc".into()), question: "q?".into() });
    assert_eq!(s.status, Some(SessionStatus::AwaitingInput));
    assert_eq!(s.pending_ask.as_deref(), Some("tc"));
    let s = SessionActor::apply_event(s, E::TurnCompleted);
    assert_eq!(s.status, Some(SessionStatus::Idle));
    let s = SessionActor::apply_event(s, E::Interrupted);
    assert_eq!(s.status, Some(SessionStatus::Interrupted));
    let s = SessionActor::apply_event(s, E::AttachFailed { error: "gone".into() });
    assert!(matches!(s.status, Some(SessionStatus::RecoveryFailed { .. })));
    let s = SessionActor::apply_event(s, E::Stopped);
    assert_eq!(s.status, Some(SessionStatus::Stopped));
}

#[tokio::test]
async fn provision_emits_create_signal_and_stop_preserves() {
    let vendor = Arc::new(MockVendor::new());
    let (sup, mut sup_rx) = test_parent(); // helper: mpsc-backed fake supervisor ActorRef? see below
    let actor = spawn_root(session_with(vendor.clone(), sup), Arc::new(InMemoryJournal::new()));
    actor.tell(SessionCommand::Provision).await.unwrap();
    let () = actor.ask(|reply| SessionCommand::Stop { reply }).await.unwrap();
    let sigs = vendor.signals();
    assert_eq!(sigs[0], format!("create:{SID}"));
    assert!(vendor.handle_signals().contains(&format!("stop:{SID}")));
}
```

Note on `test_parent()`: the parent is `ActorRef<SessionSupervisorCommand>` — build a real trivial actor? `ActorRef` can only be built by spawning an actor. Add a tiny `NullSupervisor` test actor in the tests module implementing `EventSourcedActor` with `Command = SessionSupervisorCommand`, `Event = ()`, `State = ()` that just drops commands (copy the `Parent` test-actor pattern from `actor/src/runtime.rs:526-577`). MockVendor: record handle stops in the same signals vec (`handle_signals()` can just be `signals()`; adjust assertion).

Also:

```rust
#[tokio::test]
async fn recovery_reconciles_running_to_interrupted() {
    let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
    // Incarnation 1: persist Provisioned + TurnStarted via direct journal writes
    // (simulating a mid-turn crash) — encode with serde_json like the runtime does.
    let pid = PersistenceId::new("session", SID);
    let events = vec![
        serde_json::to_vec(&SessionDomainEvent::Provisioned).unwrap(),
        serde_json::to_vec(&SessionDomainEvent::TurnStarted).unwrap(),
    ];
    journal.persist(&pid, &events).await.unwrap();
    // Incarnation 2: recovery must self-reconcile to Interrupted without vendor calls.
    let vendor = Arc::new(MockVendor::new());
    let actor = spawn_root(session_with(vendor.clone(), test_parent().0), journal);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // No vendor signal was sent during recovery (lazy).
    assert!(vendor.signals().is_empty());
    // Status is observable through a Subscribe → Status frame after reconcile,
    // or directly by folding: ask via a Get-like probe — simplest: send Stop and
    // assert prior status via the parent's received SessionStatusChanged(Interrupted).
}
```

(Refine the last assertion during implementation: the `NullSupervisor` can forward received `SessionStatusChanged` into an mpsc the test holds.)

- [ ] **Step 2: Run to verify failure** — `cargo test -p horsie-server session_actor 2>&1 | tail -5`.

- [ ] **Step 3: Implement per the tables above.** The `handle_command` UserMessage arm in full:

```rust
SessionCommand::UserMessage { text, reply } => {
    let status = state.status.clone();
    match status {
        Some(SessionStatus::Running) => {
            let _ = reply.send(Err(UserMessageError::TurnInFlight));
            CommandEffect::none()
        }
        Some(SessionStatus::AwaitingInput) if state.pending_ask.is_some() => {
            let tc = state.pending_ask.clone().unwrap_or_default();
            match self.wake(ctx, WakeMode::Attach).await {
                Ok(()) => {
                    if let Some(agent) = &self.agent {
                        let _ = agent.tell(AgentCommand::InjectToolResult {
                            tool_call_id: tc, content: text }).await;
                    }
                    let _ = reply.send(Ok(()));
                    // Idempotent resume: stay AwaitingInput until the agent's
                    // own outcome persists the next state (see workflow resume).
                    CommandEffect::none()
                }
                Err(e) => self.wake_failed(e, reply).await,
            }
        }
        None | Some(SessionStatus::Provisioning) | Some(SessionStatus::Failed { .. }) => {
            match self.wake(ctx, WakeMode::Create).await {
                Ok(()) => self.start_turn(text, reply).await,
                Err(e) => {
                    let _ = reply.send(Err(UserMessageError::RecoveryFailed(e.clone())));
                    self.report(SessionStatus::Failed { reason: e.clone() }).await;
                    CommandEffect::persist(vec![SessionDomainEvent::ProvisionFailed { error: e }])
                }
            }
        }
        Some(SessionStatus::Idle) | Some(SessionStatus::Stopped)
        | Some(SessionStatus::Interrupted) | Some(SessionStatus::RecoveryFailed { .. })
        | Some(SessionStatus::AwaitingInput) => {
            match self.wake(ctx, WakeMode::Attach).await {
                Ok(()) => self.start_turn(text, reply).await,
                Err(e) => {
                    let _ = reply.send(Err(UserMessageError::RecoveryFailed(e.clone())));
                    self.report(SessionStatus::RecoveryFailed { reason: e.clone() }).await;
                    CommandEffect::persist(vec![SessionDomainEvent::AttachFailed { error: e }])
                }
            }
        }
    }
}
```

with helpers `wake(ctx, mode) -> Result<(), String>` (ensure_runtime + ensure_agent), `start_turn(text, reply)` (agent.tell(Run) → reply Ok → report Running → persist TurnStarted). Note the `AwaitingInput` arm without pending_ask falls to the generic arm (guard order matters).

- [ ] **Step 4: Run** — `cargo test -p horsie-server 2>&1 | tail -5` → pass. `cargo clippy -p horsie-server --all-targets -- -D warnings 2>&1 | tail -3` → clean.

- [ ] **Step 5: Commit** — `git commit -am "server: SessionActor state machine with explicit vendor signals"`

---

### Task 8: Event replay — journal → wire SessionEvent with stable SSE ids

**Files:**
- Modify: `server/src/sessions/events.rs`

**Interfaces:**
- Produces (used by Task 9):

```rust
/// A coarse event replayed from the agent journal, with its stable sequence id.
pub struct StampedEvent { pub seq: u64, pub event: horsie_models::session::SessionEvent }

/// Replay the session's agent journal after `after_seq`, mapping each journaled
/// AgentDomainEvent to its wire SessionEvent. Every journal entry advances the
/// sequence counter, including entries that produce no frame (RunCancelled,
/// timer events) — ids must match journal positions exactly.
pub async fn replay_session_events(
    journal: &Arc<dyn Journal>,
    session_id: uuid::Uuid,
    after_seq: u64,
) -> Vec<StampedEvent>;
```

Mapping (exhaustive over `AgentDomainEvent`): `InputMessage{message}` / `MessageComplete{message}` → `SessionEvent::Message(MessageEvent{message})`; `ToolComplete{..}` → `ToolResult(...)`; `RunComplete{usage, iterations}` → `TurnCompleted{iterations, input_tokens: u64::from(usage.input_tokens), output_tokens: u64::from(usage.output_tokens)}` (check `Usage` field types in `models/fluorite/events.fl` or `agent.fl` first; if already u64, drop the conversion); `RunCancelled | TimerArmed | TimerCancelled | TimerFired | Parked` → no frame (seq still advances).

Implementation: replay from seq 0 always (`journal.replay(&AgentActor::persistence_id_for(session_id), 0)`), count seq from 1, skip entries with `seq <= after_seq`, decode with `serde_json::from_slice::<AgentDomainEvent>(&bytes)` — decode failures log + advance seq. **Why from 0:** `journal.replay(pid, n)` skips n entries relative to the *retained* log; interactive agents never compact (Task 2), so retained == full and replaying from 0 with our own counter is exact and simple.

Note: `AgentDomainEvent` must be deserializable here — it is exported from horsie-workflow (`pub use agent_actor::{... AgentDomainEvent ...}` already in lib.rs).

- [ ] **Step 1: Failing test** (events.rs tests):

```rust
#[tokio::test]
async fn replay_maps_and_stamps_sequential_ids() {
    let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
    let sid = uuid::Uuid::new_v4();
    let pid = horsie_workflow::AgentActor::persistence_id_for(sid);
    let msg = horsie_models::agent::Message::user("m1", "hello");
    let events = vec![
        serde_json::to_vec(&AgentDomainEvent::InputMessage { message: msg.clone() }).unwrap(),
        serde_json::to_vec(&AgentDomainEvent::RunCancelled).unwrap(),   // no frame, still counts
        serde_json::to_vec(&AgentDomainEvent::ToolComplete {
            tool_call_id: "tc".into(), output: "ok".into(), is_error: false }).unwrap(),
    ];
    journal.persist(&pid, &events).await.unwrap();
    let all = replay_session_events(&journal, sid, 0).await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].seq, 1);
    assert_eq!(all[1].seq, 3);       // RunCancelled consumed seq 2
    let after = replay_session_events(&journal, sid, 1).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].seq, 3);
}
```

- [ ] **Step 2: Run to verify failure**, **Step 3: implement**, **Step 4: run to green** — `cargo test -p horsie-server replay 2>&1 | tail -5`.

- [ ] **Step 5: Commit** — `git commit -am "server: journal replay to wire session events with stable SSE ids"`

---

### Task 9: HTTP layer — axum app, REST handlers, SSE

**Files:**
- Create: `server/src/http/mod.rs`, `server/src/http/handlers.rs`, `server/src/http/sse.rs`, `server/src/http/error.rs`
- Modify: `server/src/lib.rs` (`pub mod http;`)

**Interfaces:**
- Produces (used by Tasks 10, 12):

```rust
// http/mod.rs
#[derive(Clone)]
pub struct AppState {
    pub supervisor: ActorRef<SessionSupervisorCommand>,
    pub journal: Arc<dyn Journal>,
    pub global_events: broadcast::Sender<horsie_models::session::GlobalSessionEvent>,
    /// Finalizes a request-supplied capability spec (path expansion, plugin
    /// grants, platform seatbelt rules) — injected by the host (cli).
    pub caps_finalize: Arc<dyn Fn(CapabilitySpec) -> CapabilitySpec + Send + Sync>,
    pub default_caps: CapabilitySpec,
    pub plugins_dir: Option<PathBuf>,
    pub hook_path: Vec<PathBuf>,
}
pub fn app(state: AppState) -> axum::Router;
```

Routes (all under `/api`): `POST /sessions`, `GET /sessions`, `GET /sessions/{id}`, `POST /sessions/{id}/messages`, `POST /sessions/{id}/stop`, `DELETE /sessions/{id}`, `GET /sessions/{id}/events`, `GET /events`, `GET /health` (returns `{"ok":true}`).

**Handler behaviors:**
- `create_session`: validate `workdirs` non-empty (`422 invalid_spec`); `horsie_models::derive_workspaces(&paths)` (`422` on error); caps = `caps_finalize(req.capabilities.unwrap_or(default_caps.clone()))`; build storage `SessionSpec` (map wire `AgentSettings` → storage: `allow_ask_user: req.agent.allow_ask_user.unwrap_or(false)`, `max_retries: req.agent.max_retries.unwrap_or(0)`, vendor `req.vendor.unwrap_or_else(|| "local".into())`, plugins/hook_path from state); `supervisor.ask(Create)` → `201 CreateSessionResponse` with a summary (status Provisioning). Unknown vendor names are caught at first vendor use; optionally pre-validate: if `!state has vendor` — vendors map lives in deps not AppState; skip pre-validation (documented).
- `list_sessions` → `ask(List)` → map `(id, SessionRecord)` → `SessionSummary` (conversion fn in handlers.rs: `fn summary(id, rec) -> SessionSummary` using `status_kind()` + reason → `last_error` when RecoveryFailed/Failed carry reasons and rec-level last_error otherwise; storage `SessionRecord` has no last_error — the reason lives inside `SessionStatus::{RecoveryFailed,Failed}`; extract via helper `fn status_reason(&SessionStatus) -> Option<String>`).
- `get_session` → `ask(Get)` → `404 not_found` or `SessionDetail` (pending_question requires the child's `SessionState` — the registry doesn't hold it. Add to Task 6's supervisor `Get` reply the record only; for pending_question, extend `SessionRecord`? **Decision:** keep the registry as-is; `SessionDetail.pending_question` is filled from the *session journal* by folding `SessionState` on demand: `fold_session_state(journal, id)` helper in events.rs — replay `("session", id)` events through `SessionActor::apply_event`. Cheap and always durable-truth.)
- `send_message` → `ask(UserMessage)`; map `UserMessageError`: NotFound→404, Provisioning→409 `provisioning`, TurnInFlight→409 `turn_in_flight`, RecoveryFailed→502 `recovery_failed`. Ok → `202 AckResponse`.
- `stop` / `delete` → `ask(Stop/Delete)`; `Err(msg)` → 404 (`no such session`) else 200 `AckResponse`.
- `session_events` (sse.rs): parse `Last-Event-ID` header (`u64`, default 0). `ask(Subscribe)` → `None` → 404. Then spawn feeder task:

```rust
let (tx, rx) = tokio::sync::mpsc::channel::<Result<sse::Event, std::convert::Infallible>>(64);
tokio::spawn(async move {
    let mut last = cursor;
    // 1) replay history after the cursor
    for se in replay_session_events(&journal, sid, last).await {
        last = se.seq;
        let _ = tx.send(Ok(event_for(se))).await;   // .id(seq) + .json_data(event)
    }
    // 2) live loop
    let mut sub = sub;
    loop {
        match sub.recv().await {
            Ok(SessionFrame::Journaled) | Err(RecvError::Lagged(_)) => {
                for se in replay_session_events(&journal, sid, last).await {
                    last = se.seq;
                    if tx.send(Ok(event_for(se))).await.is_err() { return; }
                }
            }
            Ok(SessionFrame::Delta { text }) => { /* id-less Delta SessionEvent */ }
            Ok(SessionFrame::ToolStart { .. }) => { /* id-less ToolStart */ }
            Ok(SessionFrame::Status { status }) => { /* id-less StatusChanged w/ reason */ }
            Err(RecvError::Closed) => return,
        }
    }
});
Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
    .keep_alive(sse::KeepAlive::default())
```

`event_for(se)`: `sse::Event::default().id(se.seq.to_string()).json_data(&se.event)` (handle the serde error by logging + skipping — `json_data` returns Result). Id-less frames: `sse::Event::default().json_data(&SessionEvent::Delta(...))`.
- `global_events`: subscribe `state.global_events`, forward each as `sse::Event::default().json_data(&frame)`; Lagged → continue; Closed → end.
- error.rs: `pub struct Api(pub StatusCode, pub ApiError);` implementing `IntoResponse` (`(status, Json(api_error))`), constructors `not_found()`, `conflict(code, msg)`, `unprocessable(msg)`, `bad_gateway(msg)`.

- [ ] **Step 1: Failing test** — router-level smoke tests with `tower::ServiceExt::oneshot` need `tower` as a dev-dependency of horsie-server: add `tower = { version = "0.4", features = ["util"] }` under `[dev-dependencies]`. Test (http/mod.rs tests):

```rust
#[tokio::test]
async fn create_list_get_message_lifecycle_over_http() {
    let (state, _tmp) = test_state().await;   // MockVendor("mock" AND "local"), InMemoryJournal, empty providers
    let app = app(state);
    // create
    let body = serde_json::json!({
        "agent": {"model": "mock"},
        "workdirs": ["/tmp"],
        "vendor": "mock"
    });
    let res = app.clone().oneshot(post_json("/api/sessions", &body)).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created: CreateSessionResponse = read_json(res).await;
    let id = created.session.id;
    // list
    let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let list: ListSessionsResponse = read_json(res).await;
    assert_eq!(list.sessions.len(), 1);
    // get detail
    let res = app.clone().oneshot(get(&format!("/api/sessions/{id}"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // unknown session → 404
    let res = app.clone().oneshot(get("/api/sessions/does-not-exist")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    // message with no provider for model → 502 recovery_failed (mock vendor ok, provider missing)
    let res = app.clone().oneshot(post_json(&format!("/api/sessions/{id}/messages"),
        &serde_json::json!({"text": "hi"}))).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    // stop / delete
    let res = app.clone().oneshot(post_json(&format!("/api/sessions/{id}/stop"), &serde_json::json!({}))).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app.clone().oneshot(delete(&format!("/api/sessions/{id}"))).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
```

with tiny helpers `post_json`, `get`, `delete`, `read_json` built on `axum::http::Request` + `axum::body::to_bytes`. Note the "provider missing" flow requires the wake path to error before needing an LLM: `ensure_agent` fails with unknown model → the handler maps to `RecoveryFailed` → 502. Verify Task 7 orders `ensure_runtime` then `ensure_agent` — with MockVendor both signals are cheap.

- [ ] **Step 2: Run to verify failure** → **Step 3: implement** → **Step 4: green** — `cargo test -p horsie-server http 2>&1 | tail -5`.

- [ ] **Step 5: Commit** — `git commit -am "server: axum HTTP API with SSE session streams"`

---

### Task 10: `horsie serve` subcommand

**Files:**
- Create: `cli/src/serve.rs`
- Modify: `cli/src/lib.rs` (add `pub mod serve;`), `cli/src/main.rs`, `cli/Cargo.toml` (add `horsie-server = { path = "../server" }`)

**Interfaces:**
- Consumes: `HorsieConfig::resolve`, `build_registry`, `capabilities::{builtin_default, resolve_user_paths, with_plugin_grants, with_default_seatbelt_rules}`, `plugins::{plugins_dir_if_populated, resolve_hook_path}` (all existing cli items — mirror `daemon::serve` lines 74-150), plus `horsie_server::{http::{app, AppState}, sessions::{ServerDeps, SessionSupervisor}, vendor::{LocalProcessVendor, RuntimeVendor}}`.
- Produces: `pub async fn serve(cfg: HorsieConfig, addr: String) -> Result<(), CliError>`.

- [ ] **Step 1: Implement `cli/src/serve.rs`:**

```rust
//! `horsie serve`: the standalone session server (HTTP + SSE). Shares config,
//! providers, and capability resolution with the daemon, but owns a separate
//! journal root (`<data_dir>/server`) and state root (`<state_dir>/server`).

pub async fn serve(cfg: HorsieConfig, addr: String) -> Result<(), CliError> {
    let state_dir = cfg.storage.state_dir.join("server");
    let data_dir = cfg.storage.data_dir.join("server");
    std::fs::create_dir_all(&state_dir).map_err(|e| CliError::Io(e.to_string()))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| CliError::Io(e.to_string()))?;

    let registry = build_registry(&cfg)?;
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(data_dir));
    let runtime_bin = cfg.runtime.bin.clone().unwrap_or_else(default_runtime_bin);

    let default_caps = match &cfg.sandbox.capabilities_file {
        Some(path) => CapabilitySpec::load(path).map_err(CliError::Config)?,
        None => capabilities::builtin_default()?,
    };
    let plugins_dir = crate::plugins::plugins_dir_if_populated(&cfg.storage.plugins_dir);
    let hook_path = if plugins_dir.is_some() {
        crate::plugins::resolve_hook_path(cfg.runtime.hook_path.clone())
    } else { Vec::new() };
    let (pd, hp) = (plugins_dir.clone(), hook_path.clone());
    let caps_finalize: Arc<dyn Fn(CapabilitySpec) -> CapabilitySpec + Send + Sync> =
        Arc::new(move |caps| {
            capabilities::with_default_seatbelt_rules(capabilities::with_plugin_grants(
                capabilities::resolve_user_paths(caps), pd.as_deref(), &hp))
        });
    let default_caps = caps_finalize(default_caps);

    let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
    vendors.insert("local".into(), Arc::new(LocalProcessVendor::new(runtime_bin)));

    let deps = ServerDeps { provider_registry: registry, vendors, state_dir };
    let (global_tx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = spawn_root(SessionSupervisor::new(deps, global_tx.clone()), journal.clone());

    let state = AppState {
        supervisor, journal, global_events: global_tx,
        caps_finalize, default_caps, plugins_dir, hook_path,
    };
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| CliError::Executor(format!("bind {addr}: {e}")))?;
    println!("horsie server listening on http://{addr}");
    axum::serve(listener, horsie_server::http::app(state)).await
        .map_err(|e| CliError::Executor(e.to_string()))
}
```

(`default_runtime_bin` is private to `daemon/mod.rs` — make it `pub(crate)` there and reuse, or copy the 5-line helper; prefer `pub(crate)`.)

- [ ] **Step 2: main.rs subcommand:**

```rust
/// Run the session server (HTTP + SSE) in the foreground.
Serve {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Bind address for the HTTP server.
    #[arg(long, default_value = "127.0.0.1:3789")]
    addr: String,
},
```

dispatch arm:

```rust
Command::Serve { config, addr } => {
    let cfg = HorsieConfig::resolve(config.as_deref())?;
    horsie::serve::serve(cfg, addr).await?;
    Ok(0)
}
```

- [ ] **Step 3: Build + manual smoke** — `cargo build -p horsie 2>&1 | tail -3`; then `cargo run -p horsie -- serve --addr 127.0.0.1:3790 &` in a temp HOME, `curl -s http://127.0.0.1:3790/api/health` → `{"ok":true}`, `curl -s http://127.0.0.1:3790/api/sessions` → `{"sessions":[]}`, kill it.

- [ ] **Step 4: Commit** — `git commit -am "cli: horsie serve subcommand hosting the session server"`

---

### Task 11: TypeScript types package + Makefile target

**Files:**
- Create: `clients/ts/package.json`, `clients/ts/tsconfig.json`, `clients/ts/.gitignore` (`node_modules/`, `src/generated/` stays COMMITTED — generated types are the published artifact; commit them)
- Modify: `Makefile`

- [ ] **Step 1: Copy the fluorite devDependency convention** — read `/Users/xiaoguang/works/repos/bloomstack/agentx/webux/package.json`, copy its `fluorite`-providing devDependency line (whatever package supplies the `fluorite` bin) and pin the same version.

- [ ] **Step 2: `clients/ts/package.json`:**

```json
{
  "name": "@horsie/types",
  "version": "0.1.0",
  "private": true,
  "description": "TypeScript types generated from horsie's fluorite protocol schemas",
  "scripts": {
    "generate-types": "fluorite ts -i ../../models/fluorite/agent.fl ../../models/fluorite/capabilities.fl ../../models/fluorite/events.fl ../../models/fluorite/session.fl ../../models/fluorite/session_api.fl -o src/generated",
    "typecheck": "tsc --noEmit"
  },
  "devDependencies": { "<copied from agentx webux>": "<same version>" }
}
```

Include `typescript` in devDependencies (match agentx's version). `tsconfig.json`: `{ "compilerOptions": { "strict": true, "noEmit": true, "target": "es2022", "module": "esnext", "moduleResolution": "bundler" }, "include": ["src"] }`.

- [ ] **Step 3: Makefile target** (match existing target style — read the Makefile first):

```make
ts-types: ## Regenerate TypeScript protocol types from fluorite schemas
	cd clients/ts && npm install --no-audit --no-fund && npm run generate-types && npm run typecheck
```

- [ ] **Step 4: Run it** — `make ts-types`; verify `clients/ts/src/generated/session/` and `session_api/` appear and typecheck passes. If the session.fl imports (`agent.Message`) require agent.fl in the input list, it already is; if fluorite ts errors on other transitive imports (e.g. events.fl needing runtime.fl), add those to `-i` until clean.

- [ ] **Step 5: CI drift check** — read `.github/workflows/` (there is an existing CI workflow); add a job (or step) that runs `make ts-types` and then `git diff --exit-code clients/ts/src/generated` so schema/type drift fails CI. Mirror the workflow file's existing style (runner, checkout action version, caching). If node isn't available in the existing workflow, add `actions/setup-node` with a current LTS.

- [ ] **Step 6: Commit** — `git add clients/ts Makefile .github && git commit -m "clients/ts: generated TypeScript protocol types"` (commit generated output too).

---

### Task 12: Integration tests — lifecycle, SSE, restart recovery

**Files:**
- Modify: `tests/Cargo.toml` — add to `[dev-dependencies]`: `horsie-server = { path = "../server" }`, `horsie-actor = { path = "../actor", features = ["file-journal"] }`, `horsie-workflow = { path = "../workflow" }`, `horsie-models = { path = "../models" }`, `reqwest = { workspace = true, features = ["json", "stream"] }`, `tempfile = { workspace = true }`, `axum = { workspace = true }`, `uuid = { workspace = true }`, `futures-util = { workspace = true }`
- Create: `tests/tests/session_server_e2e.rs`

**Test harness** (top of file): `start_server(journal_dir: &Path, vendor: Arc<MockVendor>) -> (SocketAddr, ActorRef<SessionSupervisorCommand>, oneshot::Sender<()> /*shutdown*/)` — builds `ServerDeps` with a **real mock LLM**: `MockLlmServer` + `AnthropicProvider::with_base_url` registered as model `"mock"` (copy the provider_at pattern from `tests/tests/agent_e2e.rs:46-51`), `vendors: {"mock": vendor}`, FileJournal on `journal_dir`, spawns supervisor + `axum::serve` on `127.0.0.1:0`, returns the bound addr. Graceful shutdown: `supervisor.ask(Shutdown)` then abort the serve task.

Check `MockLlmServer`'s API in `providers/mock-llm/src/lib.rs` for scripting a simple text completion and a slow/hanging one before writing the tests; adapt the two helpers `mock_text_reply(text)` and `mock_hanging()` accordingly.

**Scenarios (each a `#[tokio::test]`):**

- [ ] **Step 1: `create_message_sse_roundtrip`** — create session (vendor "mock", model "mock"); open SSE `GET /api/sessions/{id}/events` with reqwest (stream body); POST message "hello"; assert eventually (with timeout): SSE yields an `id:`-stamped `Message` event whose message is the user "hello", then an assistant `Message`, then `TurnCompleted`; final `GET /sessions/{id}` shows `Idle`. Assert `vendor.signals()` starts `["create:<id>"]` and contains no attach.

- [ ] **Step 2: `stop_preserves_and_message_reattaches`** — after a completed turn, `POST /stop` → status Stopped, vendor saw `stop:<id>`; POST another message → 202, vendor saw `attach:<id>`, turn completes → Idle.

- [ ] **Step 3: `restart_marks_interrupted_and_message_resumes`** — script the mock LLM to hang; POST message; poll the agent journal dir until the `InputMessage` event lands (read `actors/agent/<id>/journal.jsonl` existence/size under journal_dir); shut the server core down (Shutdown + drop); start a fresh `start_server` on the SAME journal_dir; `GET /sessions` → status `Interrupted`, and `vendor.signals()` shows NO attach/create since restart (lazy). Re-script mock LLM to answer; POST message → 202 with `attach:<id>` signal; SSE (fresh connect, `Last-Event-ID: 0`) replays the interrupted history sanitized + the new turn completes → Idle.

- [ ] **Step 4: `attach_failure_lands_recovery_failed_then_retry_succeeds`** — vendor `fail_attach_times(1)`; after restart, POST message → 502; `GET /sessions/{id}` → `RecoveryFailed` with `last_error`; POST again → 202 → Idle.

- [ ] **Step 5: `last_event_id_replay_is_gap_free`** — run one full turn; collect all stamped ids from a full SSE replay; reconnect with `Last-Event-ID: <mid id>`; assert the second stream yields exactly the ids > mid, no dupes, no gaps.

- [ ] **Step 6: `turn_in_flight_conflicts`** — hanging LLM; POST message; second POST → 409.

Run: `cargo test -p integration-tests --test session_server_e2e 2>&1 | tail -15` → all pass. These tests drive real HTTP + real actors + real journals; only the sandbox process and LLM are doubled.

- [ ] **Step 7: Commit** — `git commit -am "tests: session server end-to-end lifecycle, SSE, and restart recovery"`

---

### Task 13: Final gate

- [ ] **Step 1:** `make check` (fmt + clippy + full test suite) — fix anything it surfaces.
- [ ] **Step 2:** Re-read the spec (`docs/superpowers/specs/2026-07-09-server-sessions-design.md`) section by section and confirm each requirement maps to shipped code; note deliberate deviations in the PR body (known ones: status frames are id-less on SSE — the durable status source is the session detail endpoint + supervisor journal; executor.fl AttachRuntime is a distinct wire signal whose local-vendor semantics equal a respawn).
- [ ] **Step 3:** `git push -u origin server-sessions` and open the PR (title: "server: session-oriented web backend with recoverable sessions"; body: summary, spec/plan links, test evidence, deviations). No AI attribution.
