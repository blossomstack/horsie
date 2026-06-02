# Job status command + `run` → `job run` — Design

**Date:** 2026-06-01
**Status:** Approved (pending spec review)

## Goal

Add `horsie job status <job_id>`, which shows a job's workflow execution
progress: which agents have finished, which is currently working, and which are
still pending — with per-agent and overall timing. Concurrently, move the
top-level `horsie run` command under `horsie job run` (hard move; no alias).

Target output:

```
job 3f2a… · workflow "review" · Running · 4m12s

  ✓ planner    finished   1m03s
  ▸ coder      working    3m09s
  · reviewer   pending      —
```

## Background

- Jobs are submitted to a long-lived daemon over a unix socket; the CLI's
  `run`/`job` subcommands are thin clients (`cli/src/client.rs`).
- Each job runs a `WorkflowDefinition` — a graph of agents (`planner`, `coder`, …).
- The daemon persists a per-job **workflow journal** of `WorkflowDomainEvent`s:
  `WorkflowStarted`, `AgentStarted { agent_name }`, `AgentTransitioned { from, to }`,
  `WorkflowFinished`/`Failed`/`Paused`/`Parked`/`Resumed`/`Suspended`.
- `horsie job logs` already replays this journal (`supervisor::render_history`),
  so the real execution trace is fully reconstructable daemon-side.
- The supervisor holds a `JobRecord { spec, status, submitted_at }` per job; `spec`
  contains the full `WorkflowDefinition` and the workflow name.

## Approach

Build the per-agent view from the **actual execution trace** replayed from the
workflow journal — not from a static walk of the definition's agent list.

- *Rejected: static-graph view.* Listing `def.agents` and tagging each can't
  distinguish a finished agent from a never-taken branch, and misrepresents
  loops. The journal trace is ground truth and matches the "planner finished,
  coder working" framing.
- Rows are the ordered execution trace. An agent revisited in a loop appears in
  the trace more than once — honest, and correct for the common linear case.
- After the trace, definition agents that were never visited are appended as
  `Pending`.

**Timestamps are the source of truth; durations are a display concern.** The
protocol carries only timestamps (epoch millis). All elapsed/duration arithmetic
happens at the CLI edge against the CLI's own clock. The supervisor fold stays
time-free.

## Changes

### 1. Stamp workflow events with wall-clock time

`WorkflowDomainEvent` carries no time today, and the journal layer stores no
per-event timestamp (bare serialized bytes + a derived sequence number). Add an
optional millis field to each variant, stamped at emit time:

```rust
AgentStarted     { agent_name, session_id, input, #[serde(default)] at_ms: Option<u64> }
AgentTransitioned{ from, to, from_session, to_session, condition, #[serde(default)] at_ms: Option<u64> }
WorkflowStarted  { #[serde(default)] at_ms: Option<u64> }
WorkflowFinished { output, #[serde(default)] at_ms: Option<u64> }
WorkflowFailed   { error, recoverable, #[serde(default)] at_ms: Option<u64> }
WorkflowPaused   { session_id, tool_call_id, #[serde(default)] at_ms: Option<u64> }
WorkflowParked   { session_id, #[serde(default)] at_ms: Option<u64> }
WorkflowResumed  { #[serde(default)] at_ms: Option<u64> }
WorkflowSuspended{ #[serde(default)] at_ms: Option<u64> }
```

- Stamping happens **only in the command handlers** of `WorkflowActor` (the
  side-effecting boundary), never in `apply_event` — which also runs during
  replay and would re-stamp on every recovery.
- A small private `now_ms() -> u64` helper (using `SystemTime`, no
  `unwrap`/`expect`/`panic`, mirroring the daemon's `now_millis`) supplies the
  value.
- `#[serde(default)]` keeps existing journals readable: pre-timestamp events
  deserialize with `at_ms: None`, which renders as `—`.
- `WorkflowState` is **untouched** — `at_ms` is consumed only by the progress
  fold via journal replay, not by `apply_event`. Existing `render_history`
  matches use `..` and are unaffected.

### 2. Protocol types (`fluorite/daemon.fl`)

```
/// Where an agent sits in a job's execution trace.
enum AgentPhase {
    Pending,   // defined but not yet visited
    Active,    // the current agent; qualify with JobProgress.status
    Done,      // ran and handed off (or the workflow moved past it)
}

/// One row of a job's workflow progress.
struct AgentProgress {
    name: String,
    phase: AgentPhase,
    started_at: Option<u64>,   // ms epoch; None = pending or pre-timestamp job
    ended_at: Option<u64>,     // ms epoch; None while Active/Pending
}

/// A job's workflow execution progress, for `horsie job status`.
struct JobProgress {
    job_id: String,
    workflow_name: String,
    status: JobStatus,         // overall job status; qualifies the Active row
    submitted_at: u64,         // ms epoch
    finished_at: Option<u64>,  // terminal event's timestamp; None if not terminal / pre-timestamp
    agents: Vec<AgentProgress>,
}

struct JobStatusRequest { job_id: String }

// DaemonRequest  union += JobStatus(JobStatusRequest)
// DaemonResponse union += JobProgress(JobProgress)
```

Note: the request variant is named `JobStatus` to distinguish from the existing
daemon-wide `Status` (`StatusInfo`); the response variant is `JobProgress`.

### 3. Progress fold (`supervisor/src/progress.rs`)

New module, sibling to `history.rs`, reusing the same workflow-journal replay
(promote the private `workflow_events` helper in `history.rs` to crate-visible, or
duplicate the few lines). The fold is a pure, time-free function:

```rust
pub fn fold_progress(
    events: &[WorkflowDomainEvent],
    def: &WorkflowDefinition,
    status: JobStatus,
    submitted_at: u64,
) -> JobProgress
```

Algorithm:
- Walk events in order. Each `AgentStarted { agent_name, at_ms }` opens a new
  trace row with `phase = Active`, `started_at = at_ms`, `ended_at = None`.
- The next `AgentStarted` / `AgentTransitioned` / terminal event closes the
  current row: set its `ended_at` to that event's `at_ms` and its `phase = Done`.
- After the walk, the still-open last row keeps `phase = Active` unless the job
  reached a terminal status (`Finished`/`Failed`), in which case it becomes
  `Done` with `ended_at` from the terminal event.
- Append every `def.agents` name not present in the trace as a `Pending` row
  (`started_at`/`ended_at` = `None`).
- `finished_at` = the terminal event's `at_ms` when `status` is terminal, else
  `None`.

Expose `pub use progress::fold_progress;` from `supervisor/src/lib.rs`.

To supply `def` + `submitted_at` + `status`, add a supervisor command mirroring
`List`:

```rust
SupervisorCommand::GetJob { job_id: JobId, reply: oneshot::Sender<Option<JobRecord>> }
```

`JobRecord` is already re-exported from the crate.

### 4. Daemon handler (`cli/src/daemon/mod.rs`)

Handle `DaemonRequest::JobStatus(s)`:
1. `GetJob { job_id }` → `Option<JobRecord>`. `None` ⇒ `Error("no such job: <id>")`.
2. Replay the job's workflow journal (same path as `render_history`) into
   `Vec<WorkflowDomainEvent>`.
3. `fold_progress(&events, &rec.spec.workflow, rec.status, rec.submitted_at)`.
4. Write `DaemonResponse::JobProgress(progress)`.

### 5. CLI client (`cli/src/client.rs`)

```rust
pub async fn job_status(root: &Path, job_id: String) -> Result<JobProgress, CliError>
```

Mirrors `status()`. Extend the no-wildcard `unexpected()` match and the `logs()`
stream-loop match to cover the two new response variants (the compiler enforces
this — no wildcard arms).

### 6. CLI command (`cli/src/main.rs`)

**Hard move of `run`:**
- Delete the top-level `Command::Run { … }` variant and its `dispatch` arm.
- Add `JobAction::Run { workflow, config, workdir, input, capabilities, detach }`
  with the identical doc comment and body, relocated into the `Command::Job` arm.
  `build_submit` and `client::run_attached` are unchanged.
- Update the `Command::Job` doc comment (it now also runs jobs).

**New status subcommand:**
- Add `JobAction::Status { job_id, config }`.
- Dispatch: resolve state dir → `client::job_status(&root, job_id)` → format.

Formatting (lives in `main.rs`):
- Header: `job {id} · workflow "{name}" · {status} · {humanize(overall)}`, where
  `overall = (finished_at ?? now) − submitted_at`.
- One line per agent: marker + name + label + duration.
  - marker: `Done → ✓`, `Active → ▸`, `Pending → ·`.
  - label: `Done → "finished"`; `Pending → "pending"`; `Active → ` derived from
    `JobProgress.status` (`Running → "working"`, `AwaitingUserInput → "awaiting input"`,
    `Parked → "parked"`, `Suspended → "suspended"`, `Finished → "finished"`,
    `Failed → "failed"`).
  - duration: `humanize((ended_at ?? now) − started_at)`, or `—` when
    `started_at` is `None`.
- `humanize(ms)` → compact form: `45s`, `4m12s`, `1h03m`. Lives in `main.rs`.

### 7. Docs

Sweep `README.md` and `examples/README.md` for `horsie run` and update to
`horsie job run`. Historical design docs under `docs/superpowers/specs/` are not
rewritten.

## Testing

- **Unit (`progress.rs`):** `fold_progress` over synthetic event lists —
  - linear path (`started → planner → coder → finished`): phases + timestamps;
  - a loop (an agent visited twice → two rows);
  - missing `at_ms` (old journal → `started_at`/`ended_at` `None`);
  - pending tail (definition agents never visited);
  - each terminal status (`Finished`, `Failed`) and a paused/awaiting mid-run.
- **Round-trip:** reuse the existing supervisor/CLI test harness, if present, for
  one `GetJob` + handler path; otherwise the fold unit tests carry correctness.
- Pre-PR: `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, `cargo test --workspace`.

## Out of scope

- Live-updating / watch mode for status (one-shot snapshot only).
- Backfilling timestamps onto jobs submitted before this change (they show `—`).
- Any journal-layer envelope change (timestamps are application-level only).
