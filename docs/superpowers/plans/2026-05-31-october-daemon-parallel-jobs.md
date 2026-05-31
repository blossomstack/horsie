# October Daemon & Parallel Jobs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run October as a local daemon that supervises multiple workflow jobs in parallel, auto-resumes interrupted jobs from their journals on restart, and exposes job management (list/logs/stop/resume) via a thin CLI client over a unix socket.

**Architecture:** A new transport-agnostic `supervisor` crate hosts an event-sourced `SupervisorActor` (job registry) and a per-job `JobActor` (resource lifecycle), layered over the existing `WorkflowActor`/`AgentActor`. The CLI gains a daemon host + unix-socket client. Recovery uses actor-aligned incremental persistence (a sink conduit that streams `PersistProgress` commands back to the agent actor; agentcore untouched) plus a synthetic-continue resume that synthesizes error `tool_result`s for dangling tool calls.

**Tech Stack:** Rust 2024 (toolchain 1.96.0), tokio, the in-house `actor` event-sourcing runtime, `fluorite` schema codegen, clap, serde_json, unix domain sockets, `tokio::sync::broadcast`.

**Design spec:** `docs/superpowers/specs/2026-05-31-october-daemon-parallel-jobs-design.md`

**CI gate (must stay green):** `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings` (production code denies `unwrap_used`/`expect_used`/`panic`/`wildcard_enum_match_arm`), `cargo test --locked --workspace --all-features`, `cargo deny check`. New crates MUST set `publish = false` (cargo-deny `licenses.private.ignore = true` exempts them from the license check).

---

## File Structure

**New:**
- `fluorite/daemon.fl` — wire protocol schema (requests, responses, `JobSummary`, `JobStatus`, `JobEventFrame`).
- `supervisor/Cargo.toml`, `supervisor/src/lib.rs` — crate root + exports.
- `supervisor/src/spec.rs` — `JobSpec` (storage), `JobId`, `SupervisorDeps`.
- `supervisor/src/supervisor_actor.rs` — `SupervisorActor` (registry).
- `supervisor/src/job_actor.rs` — `JobActor` (resource lifecycle; the old `drive()` assembly).
- `cli/src/daemon/mod.rs` — daemon host: builds deps, spawns supervisor, serves the control socket.
- `cli/src/daemon/protocol.rs` — length-prefixed frame codec over a unix stream (shared by host + client).
- `cli/src/client.rs` — socket client used by `run`/`job` subcommands.
- `cli/src/render.rs` — render `AgentEvent`/`JobEventFrame` to the terminal (extracted from `terminal_sink.rs`).

**Modified:**
- `models/src/lib.rs` — add `pub mod daemon { ... }`.
- `workflow/src/agent_actor.rs` — `PersistProgress`, sink conduit, split `RunReport`, `on_recovery_complete`.
- `workflow/src/workflow_actor.rs` — `on_recovery_complete`.
- `workflow/src/lib.rs` — export new symbols if needed.
- `cli/src/main.rs` — new `daemon`/`job` subcommands; `run` becomes a client; `run → job` rename.
- `cli/src/lib.rs` — declare new modules.
- `cli/src/run.rs` — gutted; assembly logic moves to `supervisor::job_actor`.
- `Cargo.toml` (workspace) — add `supervisor` member.
- `tests/Cargo.toml`, `tests/tests/daemon_e2e.rs` — integration tests.

---

## Phase 1 — Daemon wire protocol (fluorite)

### Task 1: Add the daemon protocol schema

**Files:**
- Create: `fluorite/daemon.fl`
- Modify: `models/src/lib.rs`

- [ ] **Step 1: Write `fluorite/daemon.fl`**

```fl
/// Wire protocol for the local October daemon ↔ CLI client (unix socket).
/// These are PROTOCOL types only; the persisted JobSpec is a storage type owned
/// by the supervisor crate and is intentionally not defined here.
package daemon;

use workflow.WorkflowDefinition;
use capabilities.CapabilitySpec;

/// Lifecycle status of a job, surfaced by `october job list`.
enum JobStatus {
    Running,
    Suspended,
    AwaitingUserInput,
    Finished,
    Failed,
}

/// One row in `october job list`.
struct JobSummary {
    job_id: String,
    workflow_name: String,
    status: JobStatus,
    /// Unix epoch millis when the job was submitted.
    submitted_at: u64,
    workdir: String,
}

/// Submit a new job. capabilities is the already-resolved spec (the client
/// resolves config/flags before sending) or null to use the daemon default.
struct SubmitRequest {
    workflow: WorkflowDefinition,
    workdir: String,
    input: String,
    capabilities: Option<CapabilitySpec>,
    /// A display name for listings; usually the workflow's start agent or file stem.
    workflow_name: String,
}

struct LogsRequest { job_id: String, follow: bool }
struct StopRequest { job_id: String }
struct ResumeRequest { job_id: String, message: String }

#[type_tag = "type"]
union DaemonRequest {
    Submit(SubmitRequest),
    List(EmptyRequest),
    Logs(LogsRequest),
    Stop(StopRequest),
    Resume(ResumeRequest),
    Status(EmptyRequest),
    Shutdown(EmptyRequest),
}

struct EmptyRequest {}

/// Daemon process status.
struct StatusInfo { pid: u32, uptime_secs: u64, running: u32, suspended: u32, finished: u32, failed: u32 }

/// A streamed log line for `Logs`. text is already-rendered for the terminal.
struct JobEventFrame { job_id: String, text: String }

#[type_tag = "type"]
union DaemonResponse {
    Submitted(SubmittedResponse),
    JobList(JobListResponse),
    Ack(EmptyRequest),
    Status(StatusInfo),
    LogFrame(JobEventFrame),
    /// Terminal frame of a Logs stream, or any request's failure.
    Error(ErrorResponse),
    /// Marks the end of a streaming response (Logs).
    End(EmptyRequest),
}

struct SubmittedResponse { job_id: String }
struct JobListResponse { jobs: Vec<JobSummary> }
struct ErrorResponse { message: String }
```

- [ ] **Step 2: Re-export in `models/src/lib.rs`** (after the `workflow` module block)

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod daemon {
    include!(concat!(env!("OUT_DIR"), "/daemon/mod.rs"));
}
```

- [ ] **Step 3: Build models**

Run: `cargo build -p models`
Expected: PASS (codegen picks up `daemon.fl`).

- [ ] **Step 4: Commit**

```bash
git add fluorite/daemon.fl models/src/lib.rs
git commit -m "feat(models): daemon wire protocol schema"
```

---

## Phase 2 — Incremental persistence & recovery in the workflow crate

These changes are additive and land before the supervisor so the recovery
machinery is in place. agentcore is NOT modified.

### Task 2: Split `RunReport` and add `PersistProgress`

**Files:**
- Modify: `workflow/src/agent_actor.rs`

Background (current behavior): `start_run` spawns a tokio task running
`run_with_retries`, which returns a `RunReport { events, outcome }`; the task
sends `AgentCommand::RunFinished(report)`; `handle_finished` persists
`report.events` in one batch. We change this so coarse events are streamed and
persisted as they occur, and `RunFinished` carries only the outcome.

- [ ] **Step 1: Add the `PersistProgress` command variant**

In `enum AgentCommand` add:

```rust
    /// Internal: coarse events captured mid-run, streamed for incremental
    /// persistence so a crash loses at most the in-flight message.
    PersistProgress(Vec<AgentDomainEvent>),
```

- [ ] **Step 2: Change `RunReport` to carry only the outcome**

```rust
/// Result of a background run, sent back to the actor as [`AgentCommand::RunFinished`].
/// Coarse events are streamed separately via [`AgentCommand::PersistProgress`].
pub struct RunReport {
    outcome: RunOutcome,
}
```

- [ ] **Step 3: Build a streaming conduit sink**

Replace `CapturingSink` with a `StreamingSink` that forwards every event to the
inner observation sink AND pushes coarse events to an `mpsc::UnboundedSender`
that a forwarder drains into the actor mailbox. Add near the bottom of the file:

```rust
use tokio::sync::mpsc::UnboundedSender;

/// Forwards observation events to the real sink and streams coarse domain events
/// out for the actor to persist. Never touches the journal directly.
struct StreamingSink {
    inner: Arc<dyn EventSink>,
    coarse_tx: UnboundedSender<AgentDomainEvent>,
}

impl EventSink for StreamingSink {
    fn emit(&self, event: AgentEvent) {
        if let Some(coarse) = coarse_event(&event) {
            // Unbounded: never blocks the sync emit; drained by the forwarder task.
            let _ = self.coarse_tx.send(coarse);
        }
        self.inner.emit(event);
    }
}
```

Add a single-event version alongside the existing `coarse_events`:

```rust
fn coarse_event(e: &AgentEvent) -> Option<AgentDomainEvent> {
    match e {
        AgentEvent::InputMessage(ev) => Some(AgentDomainEvent::InputMessage {
            message: ev.input.to_message(),
        }),
        AgentEvent::MessageComplete(ev) => Some(AgentDomainEvent::MessageComplete {
            message: ev.message.clone(),
        }),
        AgentEvent::ToolComplete(ev) => Some(AgentDomainEvent::ToolComplete {
            tool_call_id: ev.tool_call_id.clone(),
            output: ev.output.clone(),
            is_error: ev.is_error,
        }),
        AgentEvent::RunComplete(ev) => Some(AgentDomainEvent::RunComplete {
            usage: ev.usage.clone(),
            iterations: ev.iterations,
        }),
        AgentEvent::MessageStart(_)
        | AgentEvent::MessageStop(_)
        | AgentEvent::TextChunk(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ToolCallStart(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ToolCallInputDone(_)
        | AgentEvent::ToolExecuting(_) => None,
    }
}
```

- [ ] **Step 4: Rewrite `start_run` to stream + flush before `RunFinished`**

```rust
fn start_run(&mut self, input: AgentInput, ctx: &ActorContext<Self>, history: Vec<Message>) {
    let cancel = CancellationToken::new();
    self.running = Some(cancel.clone());

    let provider = self.ctx.provider.clone();
    let toolbox = self.ctx.toolbox.clone();
    let inner_sink = self.ctx.event_sink.clone();
    let system_prompt = self.params.system_prompt.clone().unwrap_or_default();
    let handoff_tool = self.params.handoff_tool();
    let max_iterations = self.params.max_iterations;
    let max_retries = self.params.max_retries;
    let self_ref = ctx.self_ref();

    let (coarse_tx, mut coarse_rx) = tokio::sync::mpsc::unbounded_channel();
    // Forwarder: drains coarse events and persists them through the actor, in order.
    let persist_ref = self_ref.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(ev) = coarse_rx.recv().await {
            if persist_ref
                .tell(AgentCommand::PersistProgress(vec![ev]))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let sink: Arc<dyn EventSink> = Arc::new(StreamingSink {
            inner: inner_sink,
            coarse_tx,
        });
        let outcome = run_with_retries(
            provider, toolbox, sink, system_prompt, handoff_tool,
            max_iterations, max_retries, history, input, cancel,
        )
        .await;
        // Closing the sender ends the forwarder; await it so all PersistProgress
        // commands are enqueued in the mailbox before RunFinished.
        let _ = forwarder.await;
        let _ = self_ref
            .tell(AgentCommand::RunFinished(Box::new(RunReport { outcome })))
            .await;
    });
}
```

Note: the `sink` is created inside the spawned task and the `coarse_tx` is moved
into the `StreamingSink`; the forwarder holds `coarse_rx`. Dropping the sink (end
of `run_with_retries`) closes `coarse_tx`, terminating the forwarder.

- [ ] **Step 5: `run_with_retries` returns `RunOutcome`, not `RunReport`**

Change its signature to `-> RunOutcome` and replace every `return RunReport { events, outcome }`
with `return outcome`. Delete the now-unused `events`/`coarse_events` collection
inside it (events are streamed live via the sink). The `Failed` arms that kept
captured history no longer need it — the streamed events already persisted that
history. Keep `coarse_events` only if still referenced; otherwise remove it.

- [ ] **Step 6: Handle `PersistProgress` and adjust `handle_finished`**

In `handle_command`, add:

```rust
            AgentCommand::PersistProgress(events) => CommandEffect::Persist(events),
```

`handle_finished` now takes only the outcome and must NOT re-persist events. For
`Completed`/`Concluded::Output`/`Failed` it returns `CommandEffect::Stop`
(events already persisted) instead of `PersistAndStop(report.events)`; for
`Ask` it returns `CommandEffect::Snapshot`; for `Cancelled` it persists the
single `RunCancelled` marker:

```rust
RunOutcome::Cancelled => CommandEffect::Persist(vec![AgentDomainEvent::RunCancelled]),
```

- [ ] **Step 7: Build + clippy + test**

Run: `cargo clippy -p workflow --all-targets --all-features -- -D warnings && cargo test -p workflow`
Expected: PASS. Existing `apply_event` unit tests still pass (state folding is unchanged).

- [ ] **Step 8: Commit**

```bash
git add workflow/src/agent_actor.rs
git commit -m "feat(workflow): incremental agent persistence via PersistProgress"
```

### Task 3: Synthetic-continue recovery in `AgentActor`

**Files:**
- Modify: `workflow/src/agent_actor.rs`
- Test: `workflow/src/agent_actor.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test for dangling-tool sanitization**

Add a pure helper `sanitize_for_resume(messages: Vec<Message>) -> Vec<Message>`
that appends an error `tool_result` for any `tool_use` in the final assistant
message lacking a matching result. Test:

```rust
#[test]
fn sanitize_appends_error_results_for_dangling_tool_calls() {
    let history = vec![
        user_msg("do it"),
        Message {
            id: "a".into(),
            role: Role::Assistant,
            parts: vec![
                ContentPart::ToolCall(ToolCallPart { id: "tc1".into(), name: "bash".into(), input: serde_json::json!({}) }),
                ContentPart::ToolCall(ToolCallPart { id: "tc2".into(), name: "bash".into(), input: serde_json::json!({}) }),
            ],
        },
        Message::tool_result("tc1", "ok", false),
    ];
    let fixed = sanitize_for_resume(history);
    // tc2 was dangling → an error tool_result is appended.
    let last = fixed.last().unwrap();
    match &last.parts[0] {
        ContentPart::ToolResult(r) => { assert_eq!(r.tool_call_id, "tc2"); assert!(r.is_error); }
        other => panic!("expected tool result, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p workflow sanitize_appends`
Expected: FAIL (`sanitize_for_resume` not found).

- [ ] **Step 3: Implement `sanitize_for_resume`**

```rust
/// Make a recovered history well-formed for the provider: every `tool_use` in the
/// last assistant message must have a matching `tool_result`. Any missing one (an
/// interrupted tool call) gets a synthetic error result so the model can retry.
fn sanitize_for_resume(mut messages: Vec<Message>) -> Vec<Message> {
    // Collect tool_call ids that already have results anywhere after the last assistant turn.
    let answered: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
            ContentPart::Text(_) | ContentPart::ToolCall(_) | ContentPart::Thinking(_) => None,
        })
        .collect();
    let dangling: Vec<String> = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolCall(tc) if !answered.contains(&tc.id) => Some(tc.id.clone()),
                    ContentPart::ToolCall(_)
                    | ContentPart::Text(_)
                    | ContentPart::ToolResult(_)
                    | ContentPart::Thinking(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for id in dangling {
        messages.push(Message::tool_result(
            id,
            "interrupted by shutdown, not completed",
            true,
        ));
    }
    messages
}
```

- [ ] **Step 4: Implement `on_recovery_complete`**

```rust
async fn on_recovery_complete(&mut self, state: &AgentState, ctx: &mut ActorContext<Self>) {
    // Only resume sessions that have started but not concluded. An empty history
    // means nothing ran yet (the workflow will send Run); a concluded session has
    // already stopped, so this actor wouldn't be respawned for it.
    if state.messages.is_empty() {
        return;
    }
    let history = sanitize_for_resume(state.messages.clone());
    self.start_run(
        AgentInput::user_message(new_message_id(), "continue the interrupted task"),
        ctx,
        history,
    );
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p workflow`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add workflow/src/agent_actor.rs
git commit -m "feat(workflow): synthetic-continue recovery for interrupted agents"
```

### Task 4: `WorkflowActor::on_recovery_complete`

**Files:**
- Modify: `workflow/src/workflow_actor.rs`

- [ ] **Step 1: Implement the hook**

Re-spawn the current agent child when the workflow was `Running` so the agent's
own recovery re-drives the loop. Suspended/AwaitingUserInput stay dormant.

```rust
async fn on_recovery_complete(&mut self, state: &WorkflowState, ctx: &mut ActorContext<Self>) {
    if state.status != WorkflowStatus::Running {
        return;
    }
    let (Some(agent_name), Some(session_id)) =
        (state.current_agent.clone(), state.current_session_id)
    else {
        return;
    };
    let Some(agent_def) = self.agent_def(&agent_name).cloned() else {
        return;
    };
    if let Ok(child) = self.spawn_agent(ctx, &agent_def, session_id) {
        // No command needed: the spawned AgentActor recovers its own history and
        // self-continues via its on_recovery_complete.
        self.current_child = Some(child);
    }
}
```

- [ ] **Step 2: clippy + test**

Run: `cargo clippy -p workflow --all-targets --all-features -- -D warnings && cargo test -p workflow`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add workflow/src/workflow_actor.rs
git commit -m "feat(workflow): re-spawn running agent on workflow recovery"
```

---

## Phase 3 — The `supervisor` crate

### Task 5: Scaffold the crate

**Files:**
- Create: `supervisor/Cargo.toml`, `supervisor/src/lib.rs`, `supervisor/src/spec.rs`
- Modify: root `Cargo.toml`

- [ ] **Step 1: `supervisor/Cargo.toml`**

```toml
[package]
name = "supervisor"
version = "0.1.0"
edition = "2024"
publish = false # internal application crate, never published to crates.io

[dependencies]
actor           = { path = "../actor", features = ["file-journal"] }
agentcore       = { path = "../agentcore" }
workflow        = { path = "../workflow" }
executor        = { path = "../executor" }
executor-client = { path = "../executor-client", default-features = false }
runtime-client  = { path = "../runtime-client" }
models          = { path = "../models" }
async-trait     = { workspace = true }
serde           = { workspace = true }
serde_json      = { workspace = true }
thiserror       = { workspace = true }
tokio           = { workspace = true }
tokio-util      = { workspace = true }
tracing         = { workspace = true }
uuid            = { workspace = true }

[dev-dependencies]
mock-llm = { path = "../providers/mock-llm" }
tempfile = "3"

[lints]
workspace = true
```

- [ ] **Step 2: Add `"supervisor"` to root `Cargo.toml` `members`** (after `"cli"`).

- [ ] **Step 3: `supervisor/src/spec.rs`** — storage `JobSpec` + deps

```rust
use models::capabilities::CapabilitySpec;
use models::workflow::WorkflowDefinition;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use agentcore::LlmProvider;

/// A job's unique id (a UUID string). Equals the underlying workflow run id.
pub type JobId = String;

/// Persisted, self-contained description of one job. STORAGE type (lives in the
/// supervisor journal) — distinct from the daemon wire `SubmitRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub workflow: WorkflowDefinition,
    pub workflow_name: String,
    pub workdir: PathBuf,
    pub input: String,
    pub capabilities: CapabilitySpec,
}

/// Shared, process-wide dependencies the supervisor injects into every job.
#[derive(Clone)]
pub struct SupervisorDeps {
    pub provider_registry: HashMap<String, Arc<dyn LlmProvider>>,
    pub runtime_bin: PathBuf,
    /// State root (the `.october` dir); the FileJournal is rooted here too.
    pub root_dir: PathBuf,
}
```

- [ ] **Step 4: `supervisor/src/lib.rs`**

```rust
//! Transport-agnostic supervision of parallel workflow jobs, built on the
//! event-sourced actor runtime. Shared by the CLI daemon and a future server mode.

mod job_actor;
mod spec;
mod supervisor_actor;

pub use job_actor::{JobActor, JobCommand, JobDomainEvent, JobState};
pub use spec::{JobId, JobSpec, SupervisorDeps};
pub use supervisor_actor::{SupervisorActor, SupervisorCommand, SupervisorEvent, SupervisorState};
```

- [ ] **Step 5: Build (will fail until Tasks 6-7 add the modules) — defer.** Commit scaffolding after Task 7.

### Task 6: `JobActor`

**Files:**
- Create: `supervisor/src/job_actor.rs`
- Test: same file `#[cfg(test)] mod tests`

The `JobActor` owns the executor/runtime assembly currently in `cli/src/run.rs::drive()`.
It is event-sourced: its events record lifecycle; its resources (runtime child,
socket, broadcast sender, workflow child ref) live in non-persisted fields,
re-acquired on recovery when `Running`.

- [ ] **Step 1: Types**

```rust
use crate::spec::{JobId, JobSpec, SupervisorDeps};
use actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, FileJournal, Journal, PersistenceId, spawn_root};
use async_trait::async_trait;
use models::daemon::{JobEventFrame, JobStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::broadcast;
use workflow::{WorkflowCommand, WorkflowNotification};

const LOG_BROADCAST_CAPACITY: usize = 256;

pub enum JobCommand {
    /// Begin executing the job (fresh submit).
    Start,
    /// Resume a suspended/awaiting job with a user message.
    Resume { message: String },
    /// Cancel the in-flight run (→ Suspended).
    Stop,
    /// Internal: the workflow reported a terminal/await transition.
    WorkflowEvent(WorkflowNotification),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobDomainEvent {
    JobStarted,
    JobConcluded { output: Value },
    JobSuspended,
    JobAwaitingInput,
    JobFailed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobState {
    pub status: JobStatus,
}

impl Default for JobState {
    fn default() -> Self { Self { status: JobStatus::Running } }
}
```

- [ ] **Step 2: The actor struct + resource acquisition**

Port the assembly from `cli/src/run.rs::drive()` into a method
`acquire_and_spawn_workflow(&mut self, ctx, kickoff: WorkflowCommand)` that:
builds the `ConnectedRuntimeRegistry`, binds a unique unix socket (reuse the
`socket_path()` logic — copy it into this crate), `serve_runtime_connections`,
writes the resolved capability spec to `<root>/jobs/<job_id>/capabilities.json`,
builds `ProcessRuntimeProvider::with_sandbox`, `ExecutorClient`,
`create_runtime`, `runtime_transport` → `RuntimeClient`, constructs a
`WorkflowRuntimeContext` whose `event_sink` is a broadcast-publishing sink (see
Step 3) and whose `workflow_events` mpsc is drained by a task that forwards
`WorkflowNotification` → `JobCommand::WorkflowEvent` to `ctx.self_ref()`, then
`ctx.spawn(WorkflowActor::new(job_id, def, wf_ctx))` and `tell(kickoff)`.

Keep the runtime `ExecutorClient`, `CancellationToken`, and broadcast `Sender`
in `self` so `Stop`/drop can clean up (`destroy_runtime`, `cancel`).

```rust
pub struct JobActor {
    job_id: JobId,
    spec: JobSpec,
    deps: SupervisorDeps,
    parent: ActorRef<crate::SupervisorCommand>,
    logs: broadcast::Sender<JobEventFrame>,
    // resources (non-persisted)
    workflow: Option<ActorRef<WorkflowCommand>>,
    cleanup: Option<JobCleanup>,
}

struct JobCleanup {
    client: executor_client::ExecutorClient,
    cancel: tokio_util::sync::CancellationToken,
    runtime_id: String,
}
```

- [ ] **Step 3: Broadcast-publishing sink**

```rust
struct BroadcastSink {
    job_id: String,
    tx: broadcast::Sender<JobEventFrame>,
}

impl agentcore::EventSink for BroadcastSink {
    fn emit(&self, event: agentcore::AgentEvent) {
        if let Some(text) = crate::render_event(&event) {
            let _ = self.tx.send(JobEventFrame { job_id: self.job_id.clone(), text });
        }
    }
}
```

(Provide `render_event` in `lib.rs` or inline a minimal renderer mirroring
`cli/src/terminal_sink.rs`.)

- [ ] **Step 4: `EventSourcedActor` impl**

`persistence_id` = `PersistenceId::new("job", self.job_id.clone())`.
`apply_event` maps each event to a `JobState.status`.
`handle_command`:
- `Start` → `acquire_and_spawn_workflow(ctx, WorkflowCommand::Start { input })`, then `Persist(vec![JobStarted])` + report status to parent.
- `Resume { message }` → if no live workflow, `acquire_and_spawn_workflow(ctx, WorkflowCommand::Resume { message })`; else `workflow.tell(Resume)`. `CommandEffect::None`.
- `Stop` → if live, `workflow.tell(WorkflowCommand::Cancel)`. `None` (the workflow's `Suspended` notification drives the event).
- `WorkflowEvent(n)` → translate to a `JobDomainEvent`, report `JobStatusChanged` to parent, and `Persist`/`PersistAndStop` accordingly:
  - `Finished { output }` → `PersistAndStop(vec![JobConcluded { output }])` + cleanup.
  - `Failed { error }` → `PersistAndStop(vec![JobFailed { error }])` + cleanup.
  - `Suspended` → `Persist(vec![JobSuspended])`.
  - `AwaitingUserInput { .. }` → `Persist(vec![JobAwaitingInput])`.

`on_recovery_complete`: if `state.status == Running`, `acquire_and_spawn_workflow(ctx, WorkflowCommand::Resume { message: String::new() })` — but since the workflow's own recovery re-spawns the agent which self-continues, prefer spawning the workflow with NO kickoff and letting `WorkflowActor::on_recovery_complete` drive. Implement `acquire_and_spawn_workflow_no_kick` that spawns the workflow actor without sending a command.

- [ ] **Step 5: Unit test — `apply_event` folds status**

```rust
#[test]
fn apply_event_sets_status() {
    let s = JobActor::apply_event(JobState::default(), JobDomainEvent::JobSuspended);
    assert_eq!(s.status, JobStatus::Suspended);
    let s = JobActor::apply_event(s, JobDomainEvent::JobConcluded { output: serde_json::Value::Null });
    assert_eq!(s.status, JobStatus::Finished);
}
```

- [ ] **Step 6: Commit after Task 7 builds.**

### Task 7: `SupervisorActor`

**Files:**
- Create: `supervisor/src/supervisor_actor.rs`
- Test: same file

- [ ] **Step 1: Types**

```rust
use crate::job_actor::{JobActor, JobCommand};
use crate::spec::{JobId, JobSpec, SupervisorDeps};
use actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use async_trait::async_trait;
use models::daemon::{JobStatus, JobSummary};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::sync::oneshot;

pub enum SupervisorCommand {
    Submit { spec: JobSpec, submitted_at: u64, reply: oneshot::Sender<JobId> },
    List { reply: oneshot::Sender<Vec<JobSummary>> },
    Stop { job_id: JobId },
    Resume { job_id: JobId, message: String },
    Subscribe { job_id: JobId, reply: oneshot::Sender<Option<tokio::sync::broadcast::Receiver<models::daemon::JobEventFrame>>> },
    /// Internal: a JobActor reports a status change.
    JobStatusChanged { job_id: JobId, status: JobStatus },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SupervisorEvent {
    JobSubmitted { id: JobId, spec: JobSpec, submitted_at: u64 },
    JobStatusChanged { id: JobId, status: JobStatus },
    JobRemoved { id: JobId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobRecord { spec: JobSpec, status: JobStatus, submitted_at: u64 }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SupervisorState { jobs: BTreeMap<JobId, JobRecord> }
```

`SupervisorState` derives the status registry. The actor holds a non-persisted
`BTreeMap<JobId, ActorRef<JobCommand>>` of live job children.

- [ ] **Step 2: `EventSourcedActor` impl**

- `persistence_id` = `("supervisor", "main")`.
- `apply_event` folds submit/status/remove into `jobs`.
- `handle_command`:
  - `Submit` → generate `job_id` (uuid), spawn `JobActor`, `tell(JobCommand::Start)`, store child ref, reply id, `Persist(vec![JobSubmitted { .. }])`.
  - `List` → reply with `JobSummary` rows built from `state.jobs`; `None`.
  - `Stop` → look up child, `tell(JobCommand::Stop)`; `None`.
  - `Resume` → look up child (spawn if absent for a dormant suspended job), `tell(JobCommand::Resume)`; `None`.
  - `Subscribe` → reply with a broadcast receiver from the live child (or `None`); `None`.
  - `JobStatusChanged` → `Persist(vec![SupervisorEvent::JobStatusChanged { .. }])`.
- `on_recovery_complete` → for each job whose status is non-terminal
  (`Running`/`Suspended`/`AwaitingUserInput`), spawn its `JobActor` child. Only
  `Running` jobs self-resume (the `JobActor` decides via its own
  `on_recovery_complete`); the others stay dormant until a `Resume` arrives.

Spawning a child requires the child to know its parent ref; use `ctx.self_ref()`.

- [ ] **Step 3: Unit test — registry folding**

```rust
#[test]
fn submit_then_status_updates_registry() {
    let spec = /* minimal JobSpec */;
    let s = SupervisorActor::apply_event(
        SupervisorState::default(),
        SupervisorEvent::JobSubmitted { id: "j1".into(), spec, submitted_at: 0 },
    );
    assert_eq!(s.jobs.len(), 1);
    let s = SupervisorActor::apply_event(
        s,
        SupervisorEvent::JobStatusChanged { id: "j1".into(), status: JobStatus::Finished },
    );
    assert_eq!(s.jobs.get("j1").unwrap().status, JobStatus::Finished);
}
```

- [ ] **Step 4: Build + clippy + test the crate**

Run: `cargo clippy -p supervisor --all-targets --all-features -- -D warnings && cargo test -p supervisor`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add supervisor/ Cargo.toml
git commit -m "feat(supervisor): event-sourced job supervision crate"
```

---

## Phase 4 — CLI daemon host + client

### Task 8: Frame codec

**Files:**
- Create: `cli/src/daemon/protocol.rs`
- Test: same file

- [ ] **Step 1: Length-prefixed JSON frames over a unix stream**

```rust
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Write one length-prefixed (u32 BE) JSON frame.
pub async fn write_frame<W, T>(w: &mut W, value: &T) -> std::io::Result<()>
where W: AsyncWriteExt + Unpin, T: serde::Serialize {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let len = u32::try_from(bytes.len()).map_err(std::io::Error::other)?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await
}

/// Read one frame; Ok(None) on clean EOF.
pub async fn read_frame<R, T>(r: &mut R) -> std::io::Result<Option<T>>
where R: AsyncReadExt + Unpin, T: serde::de::DeserializeOwned {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let value = serde_json::from_slice(&buf).map_err(std::io::Error::other)?;
    Ok(Some(value))
}
```

- [ ] **Step 2: Round-trip test**

```rust
#[tokio::test]
async fn frame_round_trips() {
    use models::daemon::{DaemonResponse, SubmittedResponse};
    let (mut a, mut b) = tokio::io::duplex(1024);
    let msg = DaemonResponse::Submitted(SubmittedResponse { job_id: "x".into() });
    write_frame(&mut a, &msg).await.unwrap();
    let got: DaemonResponse = read_frame(&mut b).await.unwrap().unwrap();
    assert!(matches!(got, DaemonResponse::Submitted(r) if r.job_id == "x"));
}
```

- [ ] **Step 3: Run test + commit**

Run: `cargo test -p cli frame_round_trips`
```bash
git add cli/src/daemon/protocol.rs
git commit -m "feat(cli): length-prefixed daemon frame codec"
```

### Task 9: Daemon host

**Files:**
- Create: `cli/src/daemon/mod.rs`
- Modify: `cli/src/lib.rs`, `cli/src/config.rs` (expose `build_registry` already pub)

- [ ] **Step 1: `serve` entry point**

`pub async fn serve(cfg: OctoberConfig, root_dir, runtime_bin) -> Result<(), CliError>`:
build `SupervisorDeps` (registry via `build_registry`, runtime_bin, root_dir),
`spawn_root(SupervisorActor::new(deps), Arc::new(FileJournal::new(root_dir)))`
(auto-resume fires), bind `UnixListener` at `<root>/daemon.sock` (unlink stale
first), write `<root>/daemon.pid`, accept loop: per connection read one
`DaemonRequest`, dispatch to the supervisor via a `oneshot`, write
`DaemonResponse`(s). For `Logs`, subscribe and stream `LogFrame`s + `End`.
`Shutdown` breaks the accept loop and returns.

- [ ] **Step 2: Map requests to supervisor commands**

Submit → `SupervisorCommand::Submit` (resolve `capabilities` to `CapabilitySpec`
using `capabilities::builtin_default()` when `None`, then `resolve_user_paths`),
List → `List`, Stop → `Stop`, Resume → `Resume`, Status → build `StatusInfo`,
Logs → `Subscribe` then stream.

- [ ] **Step 3: Declare `pub mod daemon;` and `pub mod client;` in `cli/src/lib.rs`.**

- [ ] **Step 4: clippy + commit**

Run: `cargo clippy -p cli --all-targets --all-features -- -D warnings`
```bash
git add cli/src/daemon/mod.rs cli/src/lib.rs
git commit -m "feat(cli): daemon host serving the control socket"
```

### Task 10: Socket client + renderer

**Files:**
- Create: `cli/src/client.rs`, `cli/src/render.rs`
- Modify: `cli/src/terminal_sink.rs` (delegate to `render`)

- [ ] **Step 1: `render.rs`** — `pub fn render_event(&AgentEvent) -> Option<String>` extracted from the existing `TerminalSink::emit` match (text → stdout-style string; tool/run notes → annotated lines). `TerminalSink` calls it. `supervisor`'s `BroadcastSink` uses the same logic (duplicate a minimal copy in supervisor to avoid a cli→supervisor dep cycle, or move the renderer into a shared low-level crate; simplest: small copy in supervisor).

- [ ] **Step 2: `client.rs`** — `connect(root_dir) -> UnixStream` (errors with "no daemon running; start it with `october daemon start`"). Helpers: `submit(stream, SubmitRequest) -> JobId`, `list`, `stop`, `resume`, `logs(follow)` (loops reading `LogFrame` until `End`/`Error`), `status`.

- [ ] **Step 3: clippy + commit**

```bash
git add cli/src/client.rs cli/src/render.rs cli/src/terminal_sink.rs
git commit -m "feat(cli): daemon socket client + shared event renderer"
```

### Task 11: Wire up subcommands + `run → job` rename

**Files:**
- Modify: `cli/src/main.rs`, `cli/src/run.rs`

- [ ] **Step 1: New `Command` enum**

```rust
#[derive(Subcommand)]
enum Command {
    Validate { #[arg(long)] workflow: PathBuf, #[arg(long)] config: Option<PathBuf> },
    /// Submit a workflow as a job to the running daemon and stream it.
    Run {
        #[arg(long)] workflow: PathBuf,
        #[arg(long)] config: Option<PathBuf>,
        #[arg(long)] workdir: PathBuf,
        #[arg(long)] input: String,
        #[arg(long)] state_dir: Option<PathBuf>,
        #[arg(long)] capabilities: Option<PathBuf>,
        /// Submit and return the job id without streaming.
        #[arg(long)] detach: bool,
    },
    /// Manage the background daemon.
    Daemon { #[command(subcommand)] action: DaemonAction },
    /// Manage jobs on the running daemon.
    Job { #[command(subcommand)] action: JobAction },
}

#[derive(Subcommand)]
enum DaemonAction {
    Start { #[arg(long)] config: Option<PathBuf>, #[arg(long)] state_dir: Option<PathBuf>, #[arg(long)] background: bool },
    Stop { #[arg(long)] state_dir: Option<PathBuf>, #[arg(long)] drain: bool },
    Status { #[arg(long)] state_dir: Option<PathBuf> },
}

#[derive(Subcommand)]
enum JobAction {
    List { #[arg(long)] state_dir: Option<PathBuf> },
    Logs { job_id: String, #[arg(long)] follow: bool, #[arg(long)] state_dir: Option<PathBuf> },
    Stop { job_id: String, #[arg(long)] state_dir: Option<PathBuf> },
    Resume { job_id: String, #[arg(short = 'm', long)] message: String, #[arg(long)] state_dir: Option<PathBuf> },
}
```

- [ ] **Step 2: Dispatch** — `Run` builds a `SubmitRequest` (load workflow, resolve capabilities to a `CapabilitySpec`), connects via client, submits, then streams logs unless `--detach`. `Daemon::Start` calls `daemon::serve` (foreground) or re-execs detached when `--background`. `Job::*` call the matching client helpers. Resolve `state_dir` → root via `OctoberConfig::resolve(config).storage.root_dir` when not given.

- [ ] **Step 3: Gut `cli/src/run.rs`** — remove `drive()`/`Manifest` (moved to supervisor). Keep nothing that duplicates the supervisor. If `run.rs` ends empty, delete it and drop its `mod` line.

- [ ] **Step 4: Full build + clippy + fmt**

Run: `cargo fmt --all && cargo clippy --all-targets --all-features -- -D warnings && cargo build --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/ && git commit -m "feat(cli): daemon + job subcommands; run submits to daemon"
```

---

## Phase 5 — Integration tests

### Task 12: End-to-end daemon tests

**Files:**
- Modify: `tests/Cargo.toml` (add `supervisor`, `cli`, `actor`, `workflow`, `models`, `tempfile`, `uuid` dev-deps)
- Create: `tests/tests/daemon_e2e.rs`

- [ ] **Step 1: Parallel submit + list**

Drive the `SupervisorActor` directly (no socket) with a `FileJournal` in a
`tempfile::TempDir` and two mock-llm-backed jobs. Submit both, assert `List`
returns two `Running`, await completion (poll `List` until both `Finished`).
Use a runtime binary built by the harness (`env!("CARGO_BIN_EXE_october-runtime")`
is unavailable cross-crate; instead pass a fake runtime or test at the
supervisor layer with a workflow whose single agent concludes via mock-llm and
needs no runtime tools). Prefer driving through `SupervisorActor` with a
no-tool workflow so no sandbox child is required.

- [ ] **Step 2: Stop → resume**

Submit a job whose agent asks the user (so it suspends as AwaitingUserInput),
assert status, `Resume`, assert it finishes.

- [ ] **Step 3: Crash/restart auto-resume**

Build a supervisor on a journal, submit a multi-turn job, drop the supervisor
(simulate crash) after the first turn persists, re-spawn `SupervisorActor` on the
same journal, assert the job auto-resumes and finishes, and assert the agent
journal contains no duplicated input message (fold and count `InputMessage`).

- [ ] **Step 4: Run + commit**

Run: `cargo test -p integration-tests`
```bash
git add tests/ && git commit -m "test: daemon parallel/resume/recovery e2e"
```

---

## Self-Review notes

- **Spec coverage:** crate (T5), actors (T6/T7), JobSpec (T5), recovery (T2/T3/T4 + T6/T7 hooks), protocol (T1), streaming logs (T6 broadcast + T9/T10), daemon lifecycle (T9/T11), CLI rename (T11), persistence layout (FileJournal kinds: `supervisor`/`job`/`workflow`/`agent`), testing (T12).
- **No journal scan:** the supervisor rebuilds its registry from its own journal (T7), matching the design (the `Journal` trait has no list API).
- **Cleanup:** `JobActor` holds `JobCleanup` and calls `destroy_runtime` + `cancel` on terminal/Stop/drop.
- **Renderer duplication:** to avoid a `cli → supervisor` dependency cycle, the small event→text renderer is copied into `supervisor`; `cli` keeps its own in `render.rs`. (If a shared crate is later warranted, extract then.)
- **clippy discipline:** no `unwrap`/`expect`/`panic` in production; exhaustive `match` (no wildcard arms) — every new enum match lists all variants.
```
