# October daemon & parallel jobs — design

**Date:** 2026-05-31
**Status:** Approved design, pre-implementation

## Goal

Run the October CLI as a long-lived local daemon that supervises multiple
workflow executions ("jobs") in parallel, survives restarts by resuming
interrupted jobs from their journals, and exposes job management (list, logs,
stop, resume) through a thin CLI client.

## Decisions (locked)

- **Interaction model:** a local background daemon owns all jobs; the CLI is a
  thin client talking to it over a unix socket.
- **Restart recovery:** on daemon start, non-terminal jobs auto-resume from
  their journals.
- **Client commands:** `list`, `logs`/tail, `stop`/cancel, `resume`/send-message.
- **Concurrency:** unbounded — every submitted job starts immediately, no queue.
- **Submission:** `october run` submits a job to the daemon and attaches
  (streams) by default; `--detach` returns the job id immediately.
- **Daemon lifecycle:** explicit start only (`october daemon start|stop|status`);
  client commands error if no daemon is running.
- **Architecture:** a new crate (Approach B), shared by the CLI daemon and a
  future server mode. The crate hosts an event-sourced supervisor actor plus a
  per-job actor that manages the resources of one execution.
- **Terminology:** full rename of `run → job` throughout (`job_id`,
  `.october/jobs/…` semantics, `JobParams`, `october job …`, etc.).
- **Recovery mechanics:** incremental (per-message) persistence + synthetic
  "continue" kick on resume; agentcore's run loop is **not** modified.
- **Dangling tool calls on resume:** synthesize error `tool_result`s for any
  unanswered tool calls so recovered history is well-formed.

## Crate: `supervisor`

A new workspace crate, `supervisor`, depending on `actor`, `workflow`,
`executor`, `models`. It contains **only** supervision logic and is
transport-agnostic — no sockets, no clap, no stdout. Both the CLI daemon and a
future `server` mode embed it via
`spawn_root(SupervisorActor::new(deps), journal)` and drive it with commands.

`Cargo.toml` sets `license` + `publish = false` (org cargo-deny requires new
crates to declare both).

The executor/runtime assembly currently inside `cli/src/run.rs::drive()` moves
into the crate's `JobActor`. The `cli` keeps config loading and provider-registry
construction, passing the built registry + runtime-binary path + default
capability spec into the supervisor as shared dependencies (`SupervisorDeps`).

## Actor hierarchy

Two new actors layered over the existing `WorkflowActor` / `AgentActor`:

```
SupervisorActor   (singleton, persistence_id = ("supervisor", "main"))
   state:   BTreeMap<JobId, JobSummary { workflow_name, status, submitted_at, workdir }>
   events:  JobSubmitted { id, spec } · JobStatusChanged { id, status } · JobRemoved { id }
   children: one JobActor per non-terminal job
        │
        ▼
JobActor          (one per job, persistence_id = ("job", job_id))
   owns OS resources: runtime child process, unix socket, ConnectedRuntimeRegistry,
                      resolved capability spec, per-job broadcast channel
   events:  JobStarted { spec } · JobConcluded { output } · JobSuspended
            · JobAwaitingInput · JobFailed { error }
   children: the existing WorkflowActor
        │
        ▼
WorkflowActor → AgentActor   (existing)
```

`job_id` is passed straight down as the `WorkflowActor`'s id (the field renamed
from `run_id` to `job_id`), giving one identity per execution:
`actors/job/<id>/`, `actors/workflow/<id>/`, `actors/agent/<session>/`.

**Why `JobActor` is its own event-sourced actor:** it survives daemon restarts
through the same replay machinery, and its journal is the home for future
per-job concerns (quotas, multiple runtimes, remote placement) without touching
workflow logic. This separates *resource lifecycle* (JobActor) from *workflow
orchestration* (WorkflowActor).

**Why the supervisor is an event-sourced registry:** the `Journal` trait has no
"list all ids of a kind" API, so the supervisor never scans disk — it replays
its own journal to know which jobs exist and their last status.

## `JobSpec` — self-contained, storage type

`JobSubmitted { spec }` carries a fully-resolved `JobSpec` into the supervisor
journal:

```
JobSpec {
    workflow_def: WorkflowDefinition,   // existing fluorite model
    workdir: PathBuf,
    input: String,
    capability_spec: CapabilitySpec,    // resolved: ~ / $HOME expanded at submit time
}
```

`JobSpec` is a **storage** struct owned by the `supervisor` crate (per CLAUDE.md:
protocol types are not storage types). It replaces today's `runs/<id>/manifest.json`
+ `capabilities.json` sidecar files — the journal becomes the single source of
truth, eliminating "files out of sync with journal" bugs.

The wire `SubmitRequest` (see Protocol) is a separate fluorite type; the daemon
translates wire → `JobSpec` (resolving capabilities) at submit time.

## Recovery

The actor runtime calls `on_recovery_complete` once after replay, before the
first live command. Recovery is purely structural:

1. `SupervisorActor::on_recovery_complete` → `ctx.spawn` a `JobActor` for every
   non-terminal job in the recovered map.
2. `JobActor::on_recovery_complete` → **only if the job was `Running`**,
   re-acquire the runtime sandbox and re-spawn the `WorkflowActor`. Jobs that
   were `Suspended` / `AwaitingUserInput` get their `JobActor` recreated but stay
   dormant (no sandbox spun up) until a `resume` arrives. This keeps the
   JobActor's resource-manager role honest and avoids booting sandbox children
   for paused jobs.
3. `WorkflowActor::on_recovery_complete` → if `Running`, re-spawn the current
   `AgentActor` child (using `current_agent` / `current_session_id` from
   recovered state).
4. `AgentActor::on_recovery_complete` → if the recovered history ends mid-turn,
   re-enter the loop (see below).

**Auto-resume applies to `Running` only.** `Suspended` (deliberate cancel or
recoverable failure) and `AwaitingUserInput` (waiting on a human) are not
auto-continued; they remain resumable on demand. "Auto-resume" means *resume
work that was interrupted*, not *restart work someone paused*.

### Incremental persistence (actor-aligned)

Today one workflow agent session = one `Run` = one full agent loop, with all
coarse events persisted in a single atomic batch at `RunFinished`. A mid-loop
crash therefore loses the entire session. To preserve mid-session progress we
persist each completed message as it is produced — **through the actor**, never
from the sink directly (persistence is the runtime's job, driven solely by
`CommandEffect::Persist` from `handle_command`; two writers to one persistence
id is forbidden).

The run loop already runs in a background tokio task (`start_run` →
`tokio::spawn`), and `handle_command(Run)` returns `CommandEffect::None`
immediately, so the actor's mailbox is free while the loop runs. Mechanism:

- A **sink conduit** wraps the real `EventSink`. On each coarse event
  (`InputMessage` / `MessageComplete` / `ToolComplete` / `RunComplete`) it
  forwards the event to a channel (`emit` is sync, so it cannot `await tell`).
- A small **forwarder task** drains the channel and `tell`s the actor a new
  `AgentCommand::PersistProgress(events)`.
- The actor handles `PersistProgress` by returning `CommandEffect::Persist(events)`
  — persistence stays exclusively in the actor, ordered through its one mailbox.
- The background loop **flushes the forwarder before sending `RunFinished`**, so
  all `PersistProgress` commands are enqueued ahead of it (mailbox order
  preserved).
- `RunFinished` no longer carries conversation events (already streamed); it
  carries only the **outcome** (`Completed` / `Concluded` / `Ask` / `Cancelled`
  / `Failed`) so the actor can notify the parent and decide `Stop` / `Snapshot`.
  `RunReport` is split into "streamed events" vs "terminal outcome".

This is contained entirely within `workflow/src/agent_actor.rs`. agentcore is
untouched.

### Synthetic-continue on resume

When `AgentActor::on_recovery_complete` finds a session that was interrupted
mid-turn, it re-enters the loop without an agentcore "continue from history"
primitive:

1. Rebuild message history by folding the journal (existing `apply_event`).
2. **Sanitize dangling tool calls:** for any `tool_use` in the trailing
   assistant message that lacks a matching `tool_result`, synthesize an error
   `tool_result` (`"interrupted by shutdown, not completed"`) so the history is
   well-formed for the provider API. The model sees the calls were interrupted
   and may retry them.
3. Send the actor itself `AgentCommand::Run { input: "continue the interrupted task" }`
   (a synthetic user message). The model sees the prior context and carries on.

`AgentInput` is unchanged (synthetic user message reuses
`AgentInput::user_message`).

### Recovery guarantee & limitation

Resume gives **at-most-one-message replay**: a crash loses at most the single
in-flight message (an LLM response or a tool execution not yet journaled).

Residual limitation: a side-effecting tool (bash, write_file) that completed but
whose `ToolComplete` was not journaled before the crash will re-execute on
resume — true exactly-once is impossible without per-tool idempotency. Future
mitigation (out of scope): a `ToolStarted` barrier event to make the ambiguous
window detectable so we can warn rather than silently re-run.

## Daemon process & lifecycle

- `october daemon start [--background]` — loads config, builds `SupervisorDeps`
  (provider registry, runtime-bin path, default capability spec),
  `spawn_root(SupervisorActor::new(deps), FileJournal)` (auto-resume fires via
  `on_recovery_complete`), binds the control socket, writes `daemon.pid` +
  `daemon.log`, serves until shutdown. `--background` re-execs detached with logs
  to `daemon.log`.
- `october daemon stop [--drain]` — sends `Shutdown`; the daemon stops accepting
  work, kills runtime children, and exits **leaving `Running` jobs in `Running`**
  so the next `start` auto-resumes them. `--drain` waits for jobs to finish
  first.
- `october daemon status` — daemon pid, uptime, job counts by status.

## Transport & protocol (fluorite)

- Unix socket at `.october/daemon.sock`, length-prefixed framed messages.
- New `.fl` schema (wire protocol):
  - `DaemonRequest`: `Submit`, `List`, `Logs { follow }`, `Stop`, `Resume`,
    `Status`, `Shutdown`.
  - `DaemonResponse`: `Submitted { job_id }`, `JobList { Vec<JobSummary> }`,
    `Ack`, `Status { … }`, `Error { message }`.
  - `JobEventFrame`: streamed log frame for `Logs`.
- `JobSummary` crosses the wire (`list`) → fluorite type.
- `JobSpec` is **storage** (supervisor journal), distinct from the wire
  `SubmitRequest`.

## Event streaming for `logs`

- Each `JobActor` owns a `tokio::sync::broadcast::Sender<JobEventFrame>`. The
  sink conduit (already in the loop for `PersistProgress`) also publishes
  render-friendly frames here for live observers.
- `logs <id>`: the daemon replays the job's workflow/agent journals to render
  history-so-far, then — if `--follow` and the job is live — subscribes to the
  broadcast channel for the tail; ends when the job is terminal or the client
  disconnects.
- `october run` (attached) = `Submit` then `Logs { follow: true }`.
- Slow consumers may observe broadcast `lagged` drops; acceptable for logs
  (noted to the user).

## CLI surface (full `run → job` rename)

- `october daemon start|stop|status`
- `october run --workflow … --workdir … --input … [--detach] [--config] [--capabilities]`
  → submits + attaches; `--detach` prints the job id and returns. Errors if no
  daemon.
- `october job list` — table: `job_id`, workflow, status, submitted_at.
- `october job logs <id> [--follow]`
- `october job stop <id>` (cancel → `Suspended`)
- `october job resume <id> [-m "message"]`
- `october validate` unchanged.
- Old `october resume --run <id>` → `october job resume <id>`.
- On-disk `.october/runs/<id>/` (manifest + capabilities sidecars) removed; the
  spec now lives in the supervisor journal.

## Job status model

`JobStatus` (fluorite, surfaced in `list`): `Running`, `Suspended`,
`AwaitingUserInput`, `Finished`, `Failed`. Derived from JobActor events; the
supervisor mirrors it in `JobSummary` via `JobStatusChanged`.

`stop` cancels the in-flight run and suspends the job (`→ Suspended`,
resumable), matching the existing `WorkflowActor` `Cancel → WorkflowSuspended`
semantics — there is no terminal "cancelled" state. Permanently abandoning a job
is `JobRemoved` at the supervisor level (future `october job remove`).

## Persistence layout (under `.october/`)

```
actors/supervisor/main/journal.jsonl
actors/job/<job_id>/journal.jsonl
actors/workflow/<job_id>/journal.jsonl
actors/agent/<session_id>/journal.jsonl
daemon.sock
daemon.pid
daemon.log
```

## Changes to existing crates

- **`supervisor` (new):** `SupervisorActor`, `JobActor`, `JobSpec`,
  `SupervisorDeps`; daemon-agnostic.
- **`workflow/src/agent_actor.rs`:** sink conduit + forwarder +
  `AgentCommand::PersistProgress`; split `RunReport` into streamed events vs
  outcome; `on_recovery_complete` → sanitize dangling tool calls + synthetic
  `Run` continue.
- **`workflow/src/workflow_actor.rs`:** `on_recovery_complete` → re-spawn current
  agent when `Running`.
- **`workflow/src/context.rs`:** `WorkflowRuntimeContext` gains a
  broadcast-publishing sink + the status mpsc the `JobActor` drains.
- **`cli`:** `drive()` logic moves into `JobActor`; `cli` becomes daemon host +
  socket client; full `run → job` rename; new `daemon` and `job` subcommands.
- **agentcore:** no changes.
- **`models` / `fluorite/`:** new `.fl` schema for the daemon wire protocol.

## Testing

- **Unit (`supervisor` crate):** `SupervisorActor` / `JobActor` `apply_event`
  folds; pure status machines.
- **Unit (`workflow`):** dangling-tool-call sanitization produces well-formed
  history; `PersistProgress` folds messages incrementally.
- **Integration (`tests/`, mock-llm):**
  - parallel submit → `list` shows both `Running` → both finish → `list`
    reflects `Finished`.
  - `stop` mid-run → `Suspended` → `resume` → completes.
  - **crash/restart:** drive a multi-turn job, drop the actor tree mid-turn,
    re-spawn the supervisor on the same journal, assert the job auto-resumes to
    completion **with no duplicated input message** in the agent history.
  - `logs`: replay history + live tail.

## Out of scope

- Concurrency caps / queueing (chosen: unbounded).
- HTTP/remote API (future server mode reuses the `supervisor` crate).
- Exactly-once tool execution / `ToolStarted` barrier.
- agentcore `continue_from_history` primitive (replaced by synthetic continue).
