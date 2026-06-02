# Job status command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `horsie job status <id>` showing per-agent workflow execution progress with timing, and move `horsie run` → `horsie job run`.

**Architecture:** Stamp workflow journal events with wall-clock millis; a pure `fold_progress` in the supervisor replays the journal into a `JobProgress` carrying timestamps; the daemon serves it over a new request; the CLI computes/render durations at the edge.

**Tech Stack:** Rust, clap, tokio, fluorite (codegen from `.fl`), event-sourced actors over a unix-socket daemon.

Spec: `docs/superpowers/specs/2026-06-01-job-status-command-design.md`.

---

### Task 1: Timestamp workflow events

**Files:**
- Modify: `workflow/src/workflow_actor.rs`

- [ ] **Step 1:** Add a `now_ms()` free fn near the top of `workflow_actor.rs`:

```rust
/// Wall-clock epoch millis for stamping events on the command path. Saturates
/// rather than panicking on a pre-epoch clock (prod lints deny panic/unwrap).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
```

- [ ] **Step 2:** Add `at_ms: Option<u64>` with `#[serde(default)]` to every `WorkflowDomainEvent` variant: `WorkflowStarted`, `AgentStarted`, `AgentTransitioned`, `WorkflowFinished`, `WorkflowSuspended`, `WorkflowFailed`, `WorkflowPaused`, `WorkflowParked`, `WorkflowResumed`. Unit variants (`WorkflowStarted`, `WorkflowSuspended`, `WorkflowResumed`) become struct variants `{ #[serde(default)] at_ms: Option<u64> }`.

- [ ] **Step 3:** Build (`cargo build -p workflow`). The compiler now flags every construction site (required field). At each site in `on_start`, `on_concluded`, `on_resume`, `on_fork`, `handle_command`, add `at_ms: Some(now_ms())`.

- [ ] **Step 4:** `apply_event` matches: `WorkflowStarted` / `WorkflowSuspended` / `WorkflowResumed` now carry a field, so change those arms to `WorkflowDomainEvent::WorkflowStarted { .. }` etc. Existing struct-variant arms already use `..` and need no change.

- [ ] **Step 5:** Update the `#[cfg(test)] mod tests` in this file: every event constructed in a test gains `at_ms: None` (or `Some(..)` where a test asserts on it — none currently do).

- [ ] **Step 6:** `cargo build -p workflow && cargo test -p workflow` → green. Commit: `feat(workflow): stamp domain events with wall-clock millis`.

---

### Task 2: Protocol types for job progress

**Files:**
- Modify: `fluorite/daemon.fl`

- [ ] **Step 1:** Add to `daemon.fl` (before the unions):

```
/// Where an agent sits in a job's execution trace.
enum AgentPhase { Pending, Active, Done }

/// One row of a job's workflow progress.
struct AgentProgress {
    name: String,
    phase: AgentPhase,
    started_at: Option<u64>,
    ended_at: Option<u64>,
}

/// A job's workflow execution progress, for `horsie job status`.
struct JobProgress {
    job_id: String,
    workflow_name: String,
    status: JobStatus,
    submitted_at: u64,
    finished_at: Option<u64>,
    agents: Vec<AgentProgress>,
}

struct JobStatusRequest { job_id: String }
```

- [ ] **Step 2:** Add `JobStatus(JobStatusRequest)` to `union DaemonRequest` and `JobProgress(JobProgress)` to `union DaemonResponse`.

- [ ] **Step 3:** `cargo build -p models` → generates `models::daemon::{AgentPhase, AgentProgress, JobProgress, JobStatusRequest}`. Commit: `feat(proto): job progress request/response types`.

---

### Task 3: `GetJob` supervisor command + `fold_progress`

**Files:**
- Modify: `supervisor/src/supervisor_actor.rs`
- Modify: `supervisor/src/history.rs` (make replay helper crate-visible)
- Create: `supervisor/src/progress.rs`
- Modify: `supervisor/src/lib.rs`

- [ ] **Step 1:** In `supervisor_actor.rs`, add a command variant:

```rust
/// Fetch one job's full record (spec + status + submit time), or `None`.
GetJob {
    job_id: JobId,
    reply: oneshot::Sender<Option<JobRecord>>,
},
```

- [ ] **Step 2:** Handle it in the command match by replying with `state.jobs.get(&job_id).cloned()` (mirror the `List` arm; it reads `state`, persists nothing — use `CommandEffect::none()`).

- [ ] **Step 3:** In `history.rs`, change `async fn workflow_events` to `pub(crate) async fn workflow_events` so `progress.rs` can reuse the exact replay path.

- [ ] **Step 4:** Create `supervisor/src/progress.rs` with the pure fold (signature below) plus unit tests. The fold takes already-replayed events (the daemon calls `workflow_events` then `fold_progress`):

```rust
use models::daemon::{AgentPhase, AgentProgress, JobProgress, JobStatus};
use models::workflow::WorkflowDefinition;
use workflow::WorkflowDomainEvent;

/// Build a `JobProgress` from a job's ordered workflow events. Pure: all duration
/// math is left to the caller (timestamps are the source of truth). Rows are the
/// execution trace (an agent revisited in a loop appears more than once), then
/// definition agents never visited, as `Pending`.
pub fn fold_progress(
    events: &[WorkflowDomainEvent],
    def: &WorkflowDefinition,
    status: JobStatus,
    submitted_at: u64,
) -> JobProgress {
    let mut rows: Vec<AgentProgress> = Vec::new();
    let mut finished_at: Option<u64> = None;

    // Close the open (last) row at `at`, marking it Done.
    fn close_last(rows: &mut [AgentProgress], at: Option<u64>) {
        if let Some(last) = rows.last_mut() {
            last.ended_at = at;
            last.phase = AgentPhase::Done;
        }
    }

    for ev in events {
        match ev {
            WorkflowDomainEvent::AgentStarted { agent_name, at_ms, .. } => {
                close_last(&mut rows, *at_ms);
                rows.push(AgentProgress {
                    name: agent_name.clone(),
                    phase: AgentPhase::Active,
                    started_at: *at_ms,
                    ended_at: None,
                });
            }
            WorkflowDomainEvent::AgentTransitioned { at_ms, .. } => {
                close_last(&mut rows, *at_ms);
            }
            WorkflowDomainEvent::WorkflowFinished { at_ms, .. }
            | WorkflowDomainEvent::WorkflowFailed { at_ms, .. } => {
                close_last(&mut rows, *at_ms);
                finished_at = *at_ms;
            }
            // Pause/Park/Suspend/Resume/Start do not close a row: the current
            // agent stays Active; the overall `status` qualifies its label.
            _ => {}
        }
    }

    // If the job is terminal, the last row is Done (closed above). Otherwise the
    // last row remains Active (its ended_at stays None → caller uses "now").
    if matches!(status, JobStatus::Finished | JobStatus::Failed) {
        if let Some(last) = rows.last_mut() {
            last.phase = AgentPhase::Done;
        }
    }

    // Append never-visited definition agents as Pending.
    for a in &def.agents {
        if !rows.iter().any(|r| r.name == a.name) {
            rows.push(AgentProgress {
                name: a.name.clone(),
                phase: AgentPhase::Pending,
                started_at: None,
                ended_at: None,
            });
        }
    }

    JobProgress {
        job_id: String::new(), // filled by the daemon
        workflow_name: String::new(),
        status,
        submitted_at,
        finished_at,
        agents: rows,
    }
}
```

- [ ] **Step 5:** Tests in `progress.rs` (`#[cfg(test)] mod tests`, with the prod-lint opt-out header used elsewhere): linear path with timestamps; a loop (agent twice → two rows); missing `at_ms` (→ `None`); pending tail; `Finished`/`Failed` terminal sets `finished_at` and last row `Done`; a mid-run `WorkflowPaused` leaves the last row `Active`. Build small `WorkflowDefinition`/event vecs inline.

- [ ] **Step 6:** In `lib.rs` add `mod progress;` and `pub use progress::fold_progress;`.

- [ ] **Step 7:** `cargo test -p supervisor` → green. Commit: `feat(supervisor): GetJob command and fold_progress`.

---

### Task 4: Daemon handler

**Files:**
- Modify: `cli/src/daemon/mod.rs`

- [ ] **Step 1:** Add to the imports `JobProgress` (and keep existing). Add a handler arm for `DaemonRequest::JobStatus(s)`:

```rust
DaemonRequest::JobStatus(s) => {
    let (tx, rx) = oneshot::channel();
    if daemon
        .supervisor
        .tell(SupervisorCommand::GetJob { job_id: s.job_id.clone(), reply: tx })
        .await
        .is_err()
    {
        write_err(&mut wr, "supervisor unavailable").await
    } else {
        match rx.await {
            Ok(Some(rec)) => {
                let events = supervisor::workflow_events(&daemon.journal, &s.job_id).await;
                let mut progress = supervisor::fold_progress(
                    &events,
                    &rec.spec.workflow,
                    rec.status.clone(),
                    rec.submitted_at,
                );
                progress.job_id = s.job_id.clone();
                progress.workflow_name = rec.spec.workflow_name.clone();
                write_frame(&mut wr, &DaemonResponse::JobProgress(progress)).await.is_ok()
            }
            Ok(None) => write_err(&mut wr, &format!("no such job: {}", s.job_id)).await,
            Err(_) => write_err(&mut wr, "job status failed").await,
        }
    }
}
```

- [ ] **Step 2:** Export `workflow_events` from supervisor: add `pub use history::workflow_events;` in `supervisor/src/lib.rs` and make the fn `pub` (not just `pub(crate)`). (Adjust Task 3 Step 3 accordingly — use `pub`.)

- [ ] **Step 3:** `cargo build -p cli` → green. Commit: `feat(daemon): serve job status`.

---

### Task 5: CLI client + command + `run`→`job run`

**Files:**
- Modify: `cli/src/client.rs`
- Modify: `cli/src/main.rs`

- [ ] **Step 1:** In `client.rs` import `JobProgress, JobStatusRequest`. Add:

```rust
pub async fn job_status(root: &Path, job_id: String) -> Result<JobProgress, CliError> {
    let resp = request(root, &DaemonRequest::JobStatus(JobStatusRequest { job_id })).await?;
    if let DaemonResponse::JobProgress(p) = resp {
        Ok(p)
    } else {
        Err(unexpected(resp))
    }
}
```

- [ ] **Step 2:** Extend the no-wildcard matches: in `unexpected()` add `DaemonResponse::JobProgress(_) => "job-progress"`; in `logs()`'s stream loop add `DaemonResponse::JobProgress(_)` to the "unexpected frame in log stream" arm.

- [ ] **Step 3:** In `main.rs`: delete `Command::Run { .. }` and its `dispatch` arm. Add a `Run { .. }` variant to `JobAction` with the identical fields and doc comment, and a `Status { job_id, config }` variant. Move the run dispatch body under the `Job` match. Update the `Command::Job` doc comment to "Run and manage jobs on the running daemon."

- [ ] **Step 4:** Add the `Status` dispatch arm + a `humanize` helper + a render fn in `main.rs`:

```rust
JobAction::Status { job_id, config } => {
    let root = resolve_state_dir(config.as_deref())?;
    let p = client::job_status(&root, job_id).await?;
    print_job_status(&p);
    Ok(0)
}
```

```rust
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `4m12s`, `45s`, `1h03m`. Input is a duration in millis.
fn humanize(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

fn print_job_status(p: &models::daemon::JobProgress) {
    use models::daemon::AgentPhase;
    let now = now_ms();
    let overall = p.finished_at.unwrap_or(now).saturating_sub(p.submitted_at);
    println!(
        "job {} · workflow \"{}\" · {:?} · {}",
        p.job_id, p.workflow_name, p.status, humanize(overall)
    );
    println!();
    for a in &p.agents {
        let (marker, label) = match a.phase {
            AgentPhase::Done => ("✓", "finished".to_string()),
            AgentPhase::Pending => ("·", "pending".to_string()),
            AgentPhase::Active => ("▸", active_label(&p.status)),
        };
        let dur = match a.started_at {
            Some(start) => humanize(a.ended_at.unwrap_or(now).saturating_sub(start)),
            None => "—".to_string(),
        };
        println!("  {marker} {:<10} {:<14} {}", a.name, label, dur);
    }
}

fn active_label(status: &models::daemon::JobStatus) -> String {
    use models::daemon::JobStatus::*;
    match status {
        Running => "working",
        AwaitingUserInput => "awaiting input",
        Parked => "parked",
        Suspended => "suspended",
        Finished => "finished",
        Failed => "failed",
    }
    .to_string()
}
```

- [ ] **Step 5:** `cargo build -p cli` → green; manual `--help` sanity (`horsie job --help` shows `run`, `status`). Commit: `feat(cli): job status command; move run under job`.

---

### Task 6: Docs + full verification

**Files:**
- Modify: `README.md`, `examples/README.md`

- [ ] **Step 1:** Replace `horsie run` occurrences with `horsie job run` in `README.md` and `examples/README.md`. Add a short `horsie job status <id>` note where `job` subcommands are documented.

- [ ] **Step 2:** Full gate:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

Fix any findings. Commit: `docs: job run / job status usage`.

---

## Notes / edge cases
- `at_ms` required at construction = compiler-enforced coverage; `#[serde(default)]` keeps old journals readable (→ `None` → `—`).
- A paused/awaiting agent's row stays `Active`; its duration grows against `now` — intended.
- Loops: an agent visited N times yields N trace rows. Honest; the common linear case looks like the spec's example.
