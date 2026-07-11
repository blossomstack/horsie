# Server Sessions Design

2026-07-09

## Goal

Turn the `server` crate into a session-oriented web backend. A user launches a
**session** — one interactive conversation with one agent, backed by one sandboxed
runtime — interacts with it over HTTP (POST messages) and SSE (live agent stream),
and can list, stop, and delete sessions in any state. Sessions and their runtimes
are persisted server-side and survive restarts; recovery failure is handled
explicitly and visibly, never silently.

## Context

- The daemon (`cli/`) already runs jobs as event-sourced actors
  (`SupervisorActor → JobActor → WorkflowActor → AgentActor`) over `horsie-actor`'s
  `FileJournal`, with structural recovery (`on_recovery_complete`) and crash-safe
  incremental agent persistence. The server reuses this machinery.
- The current `horsie-server` crate is an internal WebSocket control plane speaking
  the `executor.fl` protocol (`CreateRuntime`/`DestroyRuntime`/`RestartRuntime`/
  `QueryRuntimes`) to executor clients; it is only exercised by tests today. That
  protocol is the seed of the runtime vendor layer.
- The previous-generation agentx project provides the web shape (axum HTTP + SSE,
  `Last-Event-ID` reconnect) and the fluorite Rust↔TypeScript protocol convention.

## Decisions

| Decision | Choice |
|---|---|
| Session vs job | Session is a new first-class event-sourced actor on the shared `horsie-actor` core; jobs/daemon untouched |
| Process model | Standalone server process (`horsie serve`), own journal root, same config |
| Agent hosting | Reuse `AgentActor` from the `workflow` crate directly (no extraction) |
| Runtime vendors | Vendor-agnostic protocol at execution-sandbox altitude; agent loop stays server-side |
| Vendor scope | Local sandboxed processes (v1), containers/remote executors, E2B-class cloud sandboxes (later) |
| Recovery | Lazy for all: rebuild state at startup, no runtime respawn; wake on user action |
| Client scope | HTTP API + fluorite TS types; no web UI in this feature |
| Auth | Out of scope; server binds localhost by default |

## Architecture

```
axum HTTP/SSE  →  SessionSupervisor / SessionActor  →  AgentActor (LLM loop)
                          │                                │ tool calls
                          └── RuntimeVendor trait ─────────┴──→ vendor impls
                                                        (local process | remote | cloud)
```

Actor tree in the server process, all on `horsie-actor` event sourcing:

```
SessionSupervisor   journal: session-supervisor/main   session registry
 └── SessionActor   journal: session/<id>              lifecycle + vendor signals
      └── AgentActor journal: agent/<session-id>       conversation (reused, unchanged)
```

- `SessionSupervisor` mirrors `SupervisorActor`: journals
  `SessionCreated { spec } / SessionStatusChanged / SessionDeleted`; its folded
  state **is** the session list. No database.
- `SessionActor` mirrors `JobActor`: owns the session state machine and is the
  *only* emitter of runtime vendor signals.
- `AgentActor` is reused as-is: incremental `PersistProgress` persistence,
  dangling-tool-call sanitization, timer re-arming.
- `runtime_id == session_id` (one runtime per session, as `job_id` today).

Fluorite defines every boundary that crosses a process or language: HTTP API
types, SSE payloads, and the vendor wire protocol. Storage types (session spec
copy, records) live in the server crate, separate from protocol types.

## Session lifecycle

Status (persisted via status-change events; the list shows all states):

- `Provisioning` — vendor creating the runtime (creation provisions eagerly)
- `Idle` — ready, waiting for a user message
- `Running` — agent turn in flight
- `AwaitingInput` — agent asked the user a question (conclude/ask)
- `Interrupted` — server restarted while a turn was in flight
- `Stopped` — user stopped it; runtime stopped **but preserved**
- `RecoveryFailed { reason }` — attach/wake failed
- `Failed { reason }` — provisioning failed

**Uniform rule: a user message means "make it run."** Whatever is missing (dead
process after restart, stopped runtime, previous attach failure), a message
drives the session toward `Running`: attach or re-provision, recover agent
history, then process the message. Failure lands in `RecoveryFailed` with the
reason; the next message retries.

User action → vendor signal (the explicit-signal contract):

| User action | Session state | Vendor signal | Result |
|---|---|---|---|
| Create session | — | `create(id, spec)` | `Provisioning` → `Idle` (or `Failed`) |
| Send message | `Idle`/`AwaitingInput`, runtime live | — (tool calls flow) | `Running` → `Idle`/`AwaitingInput` |
| Send message | `Interrupted`/`Stopped`/`RecoveryFailed`, or runtime dead | `attach(id)` | recover agent → `Running` |
| Send message | `Failed` (provisioning failed) | `create(id, spec)` | retry provisioning → `Running` (or `Failed`) |
| Stop | any active | cancel turn + `stop(id)` | `Stopped`, runtime preserved |
| Delete | any | cancel turn + `delete(id)` | removed from list; runtime fate = vendor's call |
| Server restart | was `Running` | — (lazy) | `Interrupted` |
| Server restart | other states | — | unchanged |

Turn failures (LLM/provider/tool errors) do not brick sessions: an error event is
emitted on the SSE stream, `last_error` is recorded, and the session returns to
`Idle`. `Failed`/`RecoveryFailed` are reserved for the runtime/provisioning layer.

**Ask/answer**: when the agent concludes with `ask`, `SessionActor` journals
`AgentAsked { tool_call_id, question }` and enters `AwaitingInput`. The next user
message answers it: if the runtime is dead, attach first; the answer is injected
as the ask's tool result (the existing `InjectToolResult` path).

**One turn at a time**: `POST /messages` while `Running` returns `409`.

## Runtime vendor layer

Rust trait (server-side), with the wire protocol behind it:

- `create(id, RuntimeSpec) -> RuntimeHandle` — provision a new sandbox
- `attach(id) -> RuntimeHandle` — **new**: revive a preserved runtime (respawn a
  local process on its preserved workspace; resume a paused cloud sandbox; start
  a stopped container)
- `stop(id)` — **new**: halt without destroying
- `delete(id)` — session deleted; **the vendor decides** whether the underlying
  runtime is destroyed or kept
- `health(id) -> HealthStatus`
- tool execution via the existing `RuntimeClient` transport

Wire protocol: evolve `models/fluorite/executor.fl` — add `AttachRuntime`,
`StopRuntime`, `DeleteRuntime` (vendor-discretion semantics; `DestroyRuntime`
remains for forced teardown), and extend `RuntimeStateChanged` with `Stopped`.
Runtime states: `Provisioning | Running | Stopped | Failed | Deleted`.

Vendor #1 (this feature): the local process vendor wrapping today's
`ProcessRuntimeProvider` + in-memory executor transport — spawns `horsie-runtime`
under nono with the per-session capability file. Remote executors (WS) and
E2B-class cloud vendors implement the same contract later.

## HTTP API

All routes under `/api` (axum):

| Route | Purpose |
|---|---|
| `POST /api/sessions` | Create from `CreateSessionRequest { name?, spec }` → `201 Session`; provisioning async |
| `GET /api/sessions` | List all sessions, every state |
| `GET /api/sessions/{id}` | Detail: spec, status, `last_error`, timestamps |
| `POST /api/sessions/{id}/messages` | Send user message → `202`; output arrives via SSE; `409` if `Running`/`Provisioning` |
| `GET /api/sessions/{id}/events` | SSE event stream (see below) |
| `GET /api/events` | SSE of session status changes across all sessions (live list) |
| `POST /api/sessions/{id}/stop` | Stop: cancel turn + stop runtime (preserved) |
| `DELETE /api/sessions/{id}` | Delete: vendor decides runtime fate |

Error envelope: fluorite `ApiError { code, message }`; `404` unknown session,
`409` invalid state, `422` invalid spec.

### SSE event model

Two event classes, mirroring the daemon's journaling rule:

- **Durable coarse events** (`AgentMessage`, `ToolResult`, `TurnCompleted`,
  `Asked`, `StatusChanged`, `Error`) carry SSE `id:` = the agent-journal sequence
  number. The journal is the cursor store.
- **Ephemeral deltas** (streaming text chunks) are sent live without `id:` and
  are never journaled. Deltas dropped on reconnect are fine — the next coarse
  event carries the complete message.

`GET /api/sessions/{id}/events`:

- no cursor → replay all coarse events from seq 0, then stay live
- `Last-Event-ID: <seq>` header → replay after the cursor, then stay live

This one mechanism serves initial paint (replay from 0), reconnect catch-up, and
live streaming. Clients fold coarse events into a transcript (they are nearly
message-shaped: `AgentMessage` carries a full message).

**Future extension (deliberately deferred)**: backward pagination for very large
transcripts — `live=false&before=<seq>&limit=N` returning a JSON
`EventsPage { events, has_more }`. Purely additive on the same cursor scheme; add
when a real client hits real pain.

## Fluorite schemas & TypeScript

- `models/fluorite/session.fl` — `SessionSpec` (agent settings: model, system
  prompt, tool allowlist, plugins; capability spec reused from the existing
  model; workspaces; `vendor` selector defaulting to `local`), `SessionStatus`,
  `Session`/`SessionSummary`, `SessionEvent` tagged union (all SSE payloads).
- `models/fluorite/session_api.fl` — request/response types + `ApiError`.
- `models/fluorite/executor.fl` — evolved as above.
- Rust codegen: existing `models` build.rs, unchanged pattern.
- TypeScript: types-only npm package at `clients/ts/` with
  `"generate-types": "fluorite ts -i ../../models/fluorite/*.fl -o src/generated"`
  (agentx convention), a `make ts-types` target, and a CI step that regenerates
  and runs `tsc --noEmit` to catch drift. No UI in this feature.

## Persistence & recovery

**Journal root**: `$XDG_DATA_HOME/horsie/server/actors/…` — separate from the
daemon's journals; trivial to inspect or wipe. Per-session capability file at
`state_dir/server/sessions/<id>/capabilities.json` (resolved at creation; the
durable source of truth for re-attach).

**Recovery on `horsie serve` start (lazy for all):**

1. Replay the supervisor journal → session registry; spawn a `SessionActor` per
   non-deleted session (each replays its own journal; **no vendor calls, no
   agent spawn**).
2. Sessions persisted as `Running` get `StatusChanged(Interrupted)` journaled —
   the list is immediately honest.
3. If a session's journal replay fails (corrupt/unreadable), the supervisor marks
   *that session* `RecoveryFailed { reason }` and keeps serving. One bad session
   never takes the server down.

**Wake on user message:**

1. `vendor.attach(id)`; if the vendor reports the runtime gone, `create` anew on
   the preserved workspace/caps. Failure → `RecoveryFailed { reason }` persisted,
   error envelope returned, `StatusChanged` emitted on SSE.
2. Spawn `AgentActor` with **auto-resume suppressed** (see refactors): recovery
   still sanitizes dangling tool calls and re-arms timers, but does not inject
   the synthetic "continue" message — in an interactive session the user's own
   message is the continuation and becomes the run input.

**Crash safety** (inherited): journal batches are atomic (torn final writes are
dropped); agent progress is persisted incrementally, so a mid-turn crash loses at
most the one in-flight message.

## Changes to existing code

Both bounded, in the `workflow` crate:

1. **Generalize the agent's parent channel.** `AgentRuntimeContext.parent_ref` is
   `ActorRef<WorkflowCommand>` used at exactly one call site; the agent only ever
   sends the four outcome variants (`AgentConcluded`/`AgentAsked`/`AgentParked`/
   `AgentFailed`). Extract these into an `AgentOutcome` notification that both
   `WorkflowActor` and `SessionActor` can receive as parents.
2. **Auto-resume knob.** Add a flag to the agent's spawn context: workflow
   parents keep today's synthetic-continue recovery; session parents suppress it
   (sanitize + re-arm only).

The `server` crate keeps its executor WebSocket control plane (future remote
vendors) and gains the session core + axum app. The `cli` crate gains the
`horsie serve` subcommand.

## Testing

- **Unit** (alongside sources): session state-machine transitions
  (command → event → status); a `MockVendor` recording signals, asserting the
  invariant *every user action emits exactly the specified vendor signal*;
  event-fold tests.
- **Integration** (`tests/` crate; mock-llm provider + real local process vendor
  in temp dirs):
  - create → message → SSE roundtrip
  - stop → runtime preserved → message → attach → continue
  - kill the server mid-turn → restart → `Interrupted` visible → message resumes
    with sanitized history (dangling tool call answered with an error result)
  - attach failure → `RecoveryFailed` → retry succeeds
  - SSE `Last-Event-ID` replay is gap-free and duplicate-free
- **CI**: fluorite TS generation + `tsc --noEmit`; the usual `make check` gate.

## Out of scope

- Auth / multi-user (localhost binding for now)
- Web UI (TS types only)
- Backward pagination of the event stream (future extension above)
- Agent timers in sessions (`allow_timers` stays off: with lazy recovery there is
  no live actor to fire a parked session's timer after restart; revisit if
  sessions need self-waking)
- Proactive health-driven status (runtime death is discovered on next use, as the
  daemon does today; vendor health events can drive status later)
- Journal garbage collection for deleted sessions (files remain on disk, as with
  removed jobs today)
