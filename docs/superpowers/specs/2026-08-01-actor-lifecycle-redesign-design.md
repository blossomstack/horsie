# Actor lifecycle redesign — supervisor, session, agent, runtime

Status: design approved 2026-08-01. Implementation plan to follow.

## Why

The session actor tree works, but four properties block calling it production ready:

- **Cost.** Every live session pins a sandbox for as long as the process runs. Nothing is ever released without a user pressing stop.
- **Efficiency.** Every server restart re-spawns every session actor. Every `/history` read spawns a throwaway agent that never stops.
- **Determinism.** Status lives in two journals. What the model sees is repaired in memory and stored nowhere. A generation counter fences outcomes from agents that should not have existed.
- **Robustness.** Multi-minute vendor calls run on the session's mailbox, un-cancellable, blocking every other command to that session.

This redesign fixes those by moving three things: *when* actors exist, *who* owns state, and *where* the runtime lives.

## Decisions

| # | Decision |
|---|---|
| D1 | The session actor's journal is the only durable source of truth for status. The supervisor's status map is an in-memory cache, empty after a restart. |
| D2 | The supervisor persists **existence only** — created / named / deleted. |
| D3 | Nothing is loaded at boot. No boot-time re-spawn, no boot-time repair, no auto-resume ever. |
| D4 | One idle timer (configurable, default 3 min) takes a session from loaded to cold, hibernating its runtime on the way. |
| D5 | The agent is resident for the session's loaded lifetime. Transient reader agents are deleted. |
| D6 | Four working states plus one terminal: `Idle`, `Running`, `AwaitingInput`, `Failed { reason }`, `Unrecoverable { reason }`. |
| D7 | User messages are always accepted into a durable inbox. No `409`. The inbox drains at turn boundaries only. |
| D8 | The runtime is created exactly once, by one call site, at session creation. Everything later is `get`, which never creates. |
| D9 | Runtime acquisition, resume, reconnect and hibernate live in a server-level `RuntimeManager`. No actor ever holds runtime state or makes a vendor call on its mailbox. |
| D10 | No backward compatibility. Event names change freely; existing state directories are wiped on deploy. |

## Components

| Component | Kind | Owns | Durable |
|---|---|---|---|
| `SessionSupervisor` | root actor | which sessions exist; which are loaded; the idle clock | existence only: `SessionCreated`, `SessionNamed`, `SessionDeleted` |
| `SessionActor` | child, loaded on demand | conversational status, inbox, pending ask, its agents | own journal `session/<uuid>` |
| `AgentActor` | child of session | transcript, usage, timers | own journal `agent/<uuid>` |
| `RuntimeManager` | plain service, not an actor | acquisition, resume, reconnect, hibernate, delete | nothing |

The session holds `main_agent: ActorRef<AgentCommand>` and `sub_agents: HashMap<AgentId, ActorRef<AgentCommand>>`. Only `main_agent` is populated today; `sub_agents` is the axis this design deliberately preserves.

### Three tiers

- **cold** — nothing in memory. Zero cost.
- **warm** — session + main agent loaded, transcript in RAM, **no runtime**. Reading a session lands here.
- **hot** — warm, plus a live runtime.

## Lifecycle invariants

1. **Boot loads nothing but the supervisor.** No journal is read, no vendor is called.
2. **A session loads on any command addressed to it** — open, history, subscribe, message. Loading spawns the session actor and its main agent and reads two journals. It never calls a vendor.
3. **The runtime is touched only by a run that must execute.** Loading, reading and subscribing never acquire one.
4. **The idle clock runs only when no run is in flight.** A forty-minute `bash` cannot be offloaded out from under itself. A session parked on a pending ask *is* offloadable — the ask is durable.
5. **Offload is an acked sequence:** supervisor sends `PrepareOffload` → the session refuses if a run started meanwhile (clock resets) → otherwise it stops its agents, calls `runtimes.hibernate(...)`, and its ack is emitted in the same effect as its stop, so it writes nothing afterwards. The supervisor drops its `ActorRef` on the ack. Anything arriving later loads a fresh actor. Two actors never share a persistence id.
6. **Only the actor owning a persistence id touches that journal.** The supervisor never reads a session's log; the session never reads the agent's.

Offload and reload cost is a function of the journal implementation. `FileJournal` has no snapshotting today, so a reload replays the whole transcript; the timer is therefore configurable, and making reload cheap is the journal's problem to solve (see Out of scope).

## Session state machine

```rust
struct SessionState {
    status: SessionStatus,            // Idle | Running | AwaitingInput | Failed | Unrecoverable
    pending_ask: Option<AskRecord>,   // tool_call_id + question
    inbox: Vec<InboxMessage>,         // accepted, not yet delivered to a turn
    agent_usage: HashMap<AgentId, UsageTotal>,
    last_error: Option<String>,
}
```

### Events

| Event | Fold |
|---|---|
| `MessageQueued { id, text }` | append to `inbox`; status unchanged |
| `TurnBegan { consumed: Vec<MsgId>, answering: Option<ToolCallId> }` | `Running`; remove `consumed` from inbox; clear `pending_ask` |
| `AskRecorded { tool_call_id, question }` | `AwaitingInput`; set `pending_ask` |
| `TurnEnded` | `Idle` |
| `TurnFailed { error }` | `Failed { reason }`; set `last_error` |
| `TurnStopped` | `Idle` |
| `TurnInterrupted` | `Idle`; recovery repair only |
| `SessionFailed { reason }` | `Unrecoverable`; terminal |
| `UsageRecorded { agent_id, usage_total }` | unchanged from today |

### Rules

**Accepting input.** A message is always accepted — `202` with its id, never `409`. It becomes `MessageQueued`. If the session is idle the drain fires at once; if a turn is running it waits.

**Draining.** The inbox drains **only at a turn boundary**, never at load time. `TurnEnded` and `TurnStopped` drain; `TurnFailed` does not, so a stuck cause (expired key, dead vendor) cannot turn three queued messages into three back-to-back failures. A session sitting in `Failed` with a non-empty inbox drains everything — the held messages plus the new one — on the user's next message. After a restart the messages sit in the inbox, the client renders them unread, and they go in with whatever turn starts next.

**Merging.** Multiple inbox entries become **one** user message, joined in arrival order with a blank line. Anthropic requires alternating roles, so consecutive user messages are not portable. Provenance survives: the session journal keeps each `MessageQueued`, so the UI can show three bubbles though the model saw one message.

**Answering an ask.** No separate path. A message arriving while `AwaitingInput` is queued like any other; when the turn begins, the merged text is delivered as the `tool_result` for `pending_ask` instead of as a fresh user message. `TurnBegan` consumes the messages and clears the ask in one journaled step, so a crash anywhere in that window replays deterministically.

**Stop.** Cancels the current turn and nothing else: tell the runtime to abandon in-flight work, cancel the run, wait for the ack, journal `TurnStopped` → `Idle`. The inbox is untouched, so queued messages start the next turn immediately. This is why the unread markers matter in the client: without them, stop-then-immediately-running looks like a bug.

**Failure taxonomy.**
- `Failed { reason }` — the **last turn** failed: provider error, tool failure, vendor unavailable, provisioning failure. Sticky so the UI can badge it, fully recoverable — the next turn moves it to `Running`.
- `Unrecoverable { reason }` — terminal, read-only. Today the only reason is `RuntimeGone`. The exit is a new session (session fork is out of scope).

**Recovery.** On load, fold the journal. If `status == Running`, no run can exist, so journal `TurnInterrupted` → `Idle` before answering anything. The inbox is untouched. Nothing runs. A session parked on `pending_ask` stays parked.

## RuntimeManager

```rust
impl RuntimeManager {
    // exactly one call site: the session actor, at session creation
    async fn create(&self, session: SessionId, vendor: &str, spec: &SessionSpec) -> Result<(), RuntimeError>;

    // every later use. Pure lookup plus vendor-side resume. Never creates.
    async fn get(&self, session: SessionId, vendor: &str) -> Result<RuntimeClient, RuntimeError>;

    async fn hibernate(&self, session: SessionId, vendor: &str);
    async fn delete(&self, session: SessionId, vendor: &str);
}

impl RuntimeClientProvider {          // handle bound to (session_id, vendor)
    async fn get(&self) -> Result<RuntimeClient, RuntimeError>;
}
```

**Created once, structurally.** One call site calls `create`, and it runs once in a session's life. There is no bookkeeping table and no derived truth — the guarantee is the call graph. In the web client the first message is what creates the session (`NewSessionView.handleSend` creates, then navigates, then sends), so this is also when the user first pays for a sandbox.

**Timing.** `create` is started at session creation but not awaited on the HTTP path. `POST /sessions` returns immediately; the first turn's `get()` waits for creation to resolve — that wait is the *vendor's* obligation under the contract below, not something the manager arranges — and provisioning progress frames stream meanwhile. No `Provisioning` status is needed, no vendor call sits on a mailbox, and a failed create surfaces as a failed first turn.

**Resume never re-provisions.** Creating is a one-time event. A vendor that cannot resume must report an error rather than silently building a fresh sandbox — a silent re-clone destroys work the user believes still exists.

**Reconnect is internal.** `is_connected` disappears above the manager. A request against a dead socket re-establishes the transport; only if the sandbox itself is unrecoverable does that surface, as `RuntimeGone`. Two guards keep mid-flight tool loss out of normal operation: a transport keepalive so a dead socket is detected rather than discovered mid-call, and invariant 4 (a session with a run in flight is never offloaded). A tool result lost to a hard crash stays lost; a possibly half-executed `bash` is never replayed.

**No vendor call on a mailbox.** The agent awaits `get()` inside its run task, under the run's cancel token, so a stuck provision is cancellable by stop and cannot wedge the session.

**Errors the actors see:**
- `VendorUnavailable` — vendor not registered, or its daemon is not connected. Retryable → `Failed`. A local daemon offline for ten minutes must never kill a session permanently.
- `RuntimeGone` — a live vendor says this session's runtime cannot be produced. Terminal → `Unrecoverable`.

Only a live vendor can declare `Gone`; absence never can.

**Spec assembly** — writing the capability file, minting the scoped GitHub token, resolving plugin bundles into env — moves out of `ensure_runtime` into the manager, which re-assembles it fresh per acquisition (tokens are short-lived and must not be cached). The session actor is left with no vendor knowledge at all.

### Vendor contract

Vendors are external agent processes speaking the WebSocket protocol in `models/fluorite/runtime_vendor.fl`; the server's end is `RuntimeVendorLink`, and the reusable agent half is the `runtime-vendor` crate. The redesign changes two commands:

| Today | After | Meaning |
|---|---|---|
| `CreateRuntime { runtime_id, spec }` | unchanged | provision a brand-new runtime |
| `AttachRuntime { runtime_id, spec }` | **`GetRuntime { runtime_id }`** | return this runtime, resuming it if hibernated. **Never creates**, and takes no spec — the vendor already holds it from create, and accepting one is what makes silent re-provisioning possible |
| `StopRuntime { runtime_id }` | **`HibernateRuntime { runtime_id }`** | advisory: suspend if you can, otherwise do nothing and keep the runtime |
| `DeleteRuntime`, `QueryRuntimes`, `Runtime` relay | unchanged | |

- **`hibernate` is advisory.** A vendor that cannot suspend implements it as a no-op and keeps the sandbox alive. That is the honest answer for the process-backed agent today, and for any container agent whose stop destroys the workspace.
- **`GetRuntime` fails rather than provisions.** A vendor with no live runtime for that id replies `RequestFailed`; the server maps that to `RuntimeGone`, and the session becomes `Unrecoverable`. This is the single rule that keeps a silent re-clone impossible.
- **Concurrency is the vendor's responsibility.** `GetRuntime` must not answer before an in-flight `CreateRuntime` for the same id resolves, whether it succeeded or failed; concurrent gets for one id must never produce two sandboxes. The manager does no joining or deduplication.
- A **vendor conformance suite** (same shape as the OpenAI wire conformance tests from #24) pins these behaviours, run against `FakeRuntimeVendor` and the real agent loop.

## Agent actor

- **Resident.** Spawned when the session loads, stopped when it unloads. `handle_finished` no longer stops the actor on `Concluded` / `Failed`; it reports and goes idle.
- **The runtime arrives through a provider, never a captured client.** `SessionContextProvider` holds a `RuntimeClientProvider` and calls `.get()` per run. This single change dissolves the chain that produced per-turn respawns, stragglers, and the generation fence.
- **`generation` becomes `run_id`, inside the agent.** Each run has an id; `RunFinished` carries it; a report whose id is not the current run is dropped by the agent. The session fences nothing and can trust every outcome it receives. `SessionParent` loses its generation field.
- **No transient readers.** `GetHistory` / `GetUsage` are answered by the resident agent from memory. `NoContextProvider` and both spawn sites are deleted.
- **Cancel keeps its ack.** Stop needs "the run is really over" before the session records a turn boundary.
- **Repair is renamed and persisted.** `sanitize_for_resume` / `sanitize_answering` become `repair_unanswered_tool_calls` / `repair_unanswered_tool_calls_except(answering)`. The synthetic `tool_result`s are journaled **once**, when the turn is known to be over — on `RunCancelled` and on `TurnInterrupted` during recovery — instead of being recomputed on a clone at every turn start. The journal then holds exactly what the model will be sent, and the client can render "tool call interrupted" honestly.

## Deletions

`Stopped`, `Interrupted`, `RecoveryFailed`, `Provisioning`; `WakeMode`; `attach` (its "provision a fresh instance against the same spec" fallback is exactly the silent re-provision this design bans); `ensure_runtime` / `ensure_agent` as session methods; the generation fence; `is_connected` above the manager; `NoContextProvider` and both transient-reader spawn sites; boot-time session re-spawn; the `409 TurnInFlight` path; the dead `SessionEvent::Asked` wire variant; every legacy session event variant.

## Wire and client

- `SessionStatusKind` drops `Stopped` / `Interrupted` / `RecoveryFailed`, gains `Unrecoverable`.
- `POST /messages` returns `202` with a message id; `409` is gone.
- Session detail carries the inbox; SSE gains queued/consumed events so a second tab stays honest.
- The client renders queued messages as unread, renders `Unrecoverable` read-only, and no longer handles `409`.

## Testing

The fault-injection testkit from #68 (`agentcore::testkit`, `Script<T>`) plus `MockVendor`'s signal recording covers most of this. **The idle timer must be driven by an injectable clock**, not sleeps, or every lifecycle test is a three-minute test or a flake.

Invariants pinned by tests, each of which fails against today's code:

1. Boot with N sessions on disk → zero journal reads, zero vendor signals, until one is opened.
2. Open, read history, subscribe → zero vendor signals.
3. Clock past the timer → exactly one `hibernate`; the next message → `get`, never `create`.
4. Run in flight past the timer → no `hibernate`.
5. Message during `Running` → `202` and queued; turn ends → one turn carrying both texts merged.
6. Crash mid-turn → reload → inbox intact, status `Idle`, nothing running.
7. Failed turn → inbox not drained. Stop → inbox drained into the next turn.
8. `RuntimeGone` → `Unrecoverable` and terminal. `VendorUnavailable` → `Failed` and retryable.
9. Twenty turns and three offloads → exactly one `create`.
10. Crash mid-tool-call → reload → synthetic `tool_result`s journaled once, not re-added on later turns.
11. Vendor conformance: a `get` issued during an in-flight `create` blocks until it resolves; concurrent `get`s yield one sandbox.

## Out of scope

- **`FileJournal` snapshotting** (#61 items 9, 13). An infra decision behind the `Journal` trait; reload cost is its problem.
- **Real velos hibernate/resume.** Until it exists, velos implements `hibernate` as a no-op and keeps the container, so the memory win lands now and the sandbox-cost win lands later.
- **Session fork** — the exit from `Unrecoverable`. Needs a working `copy_snapshot`.
- **Credential refresh across resume** — #96. A resumed sandbox keeps the environment it was born with, so a long-hibernated runtime can hold an expired GitHub token.
