# Observable session resources + history pagination

2026-07-21

## Goal

Let a user reopen a session with arbitrarily long history and (a) see the
latest conversation without replaying the whole transcript, scrolling up to
load older messages on demand, and (b) see — live *and* in history — the
progression of the resource setup behind each run ("runtime initializing →
ready", "scanning workspace", "runtime re-initializing" after a suspend).

Backward pagination was named and deliberately deferred in the 2026-07-09
sessions design ("add when a real client hits real pain"). Scoping it surfaced
deeper structural issues that this design fixes together, because pagination
done right depends on them.

## Problems in today's code

- **Server reads actor journals directly.** `replay_session_events`
  (`server/src/sessions/events.rs`) walks the agent journal and deserializes
  `AgentDomainEvent` in the server crate. SSE initial paint replays the *entire*
  journal from seq 0 on every open. The task-list/usage widgets only work
  because of that full replay.
- **Heavy IO on the actor mailbox.** `ensure_agent`/`ensure_runtime`
  (`server/src/sessions/session_actor.rs`) `await` sandbox round-trips
  (workspace scan, SessionStart hook), MCP connects, and `vendor.create/attach`
  *inline on the `SessionActor` mailbox*, blocking it from servicing SSE
  subscribe / stop / status while a turn spins up.
- **Resources baked at spawn; system prompt recomputed every turn.** The agent
  is dropped after each concluded turn (`self.agent = None`), so the next
  message respawns it and re-runs the full scan + system-prompt compose —
  recomputing a prompt that should be stable for the conversation.
- **Setup is invisible.** The user sees nothing while a runtime rehydrates or a
  workspace scans; there is no audit of it afterward.

## Principles (settled in design)

1. **Actor encapsulation.** Only an actor reads its own journal/state. Everyone
   else sends a command. The server never touches a journal.
2. **Transparent recovery.** How an actor obtains its current state — journal
   replay today, a persisted snapshot tomorrow — is an implementation detail
   invisible to callers. Callers assume the state is simply *there*.
3. **Quick handlers, heavy work off-mailbox.** A command handler validates and
   dispatches; sandbox round-trips / connects / LLM loops run on spawned tasks
   and report back via follow-up commands.
4. **Idempotent per-run ensure, not redo-all.** Each run ensures its resources
   are ready ("initialize if not initialized" — rehydrate a suspended runtime,
   reconnect a dropped MCP). Cheap when already live. The system prompt is
   *once per conversation*, not per run.
5. **Observable + auditable prep.** Every resource progression is both streamed
   live and journaled, so it appears during the wait and remains in history.
6. **Agents own their state.** Messages, tasks, usage, and the system prompt
   belong to the agent — which generalizes to multiple agents per session
   later, each owning its own.

## Architecture

### Actor tree

```
SessionActor      event-sourced: session lifecycle + resource progressions
 │                (journaled = audit) + aggregated live resource state
 ├── AgentActor    event-sourced: conversation — messages, tasks, usage,
 │                 system prompt
 ├── RuntimeActor  non-journaling: owns the sandbox lifecycle + hands out
 │                 runtime_client (tool calls / scan / SessionStart run over it)
 └── McpActor      non-journaling: owns MCP connections
     (+ future resource actors)
```

**What warrants an actor:** something that *owns maintained, independently-
lifecycled live state*. The runtime qualifies (create/attach/suspend/resume/
health; it can re-initialize mid-session on its own) and MCP qualifies
(connections drop, reconnect, health-check). The **workspace scan /
SessionStart does not** — it is a stateless one-shot that reads the sandbox
once and produces a value (the system prompt → agent state), entirely
dependent on the runtime. So it is *not* a separate actor: it is an operation
performed over the `RuntimeActor`'s `runtime_client`, run off-mailbox as a step
in the prep coordination. (Same reason live `skill`/`inspect_workspace`
re-scans go straight over `runtime_client`, not through any scanner actor.)

**Non-journaling actors** are `EventSourcedActor`s with `Event = ()` that only
ever return `CommandEffect::none()` / reply effects (the framework already
supports this — see the test actor at `actor/src/runtime.rs:541`; no new
primitive needed). Their *live* state (a socket, a sandbox process handle) is
intentionally ephemeral: on restart they recover to `initial_state()` and
rebuild it — you journal the story, not the socket. The durable **audit** of
their transitions lives in the `SessionActor` journal, not their own.

### Resource actors + observable prep

- Each resource actor owns its volatile state and does its heavy async work on
  its **own spawned task**, off any shared mailbox.
- Progressions come from **two** sources, both flowing to `SessionActor`:
  resource actors emit *lifecycle* progressions (`Initializing`, `Ready`,
  `Suspended`, `Reinitializing`, `Failed`), and the prep coordination emits
  *operation* progressions ("scanning workspace → scanned", "building toolbox")
  for the steps that are not a resource's own lifecycle. Not every progression
  is a resource-lifecycle transition, and this keeps the scan where it belongs
  (an operation, not an actor).
- `SessionActor`, per progression message: (a) journals a progression event
  (audit), (b) rebroadcasts it to the SSE frame stream (live), (c) updates its
  aggregated resource-state view. All three are quick — no IO on this mailbox.
- **Per-run prep** is an async coordination on a spawned task that walks the
  dependency graph: ensure `RuntimeActor` ready (→ obtain `runtime_client`) →
  ensure `McpActor` ready → build the toolbox → (first turn only) scan the
  workspace + SessionStart over `runtime_client` to compose the system prompt.
  Resource *ensures* are idempotent commands to the resource actors (an
  already-live resource replies immediately); the scan and toolbox build are
  operations the coordination performs directly. Complex dependencies are just
  this ordering.
- **Extensibility:** a new resource type is a new resource actor plus a node in
  the prep coordination. The agent depends only on the *prepared run* handed to
  it; what that prep is composed of is open-ended and owned by the session
  layer.

### Agent resource-decoupling

- `AgentActor` holds **no** provider/toolbox as identity. Its context is
  `event_sink`, `parent`, `session_id`, and a prep handle. Spawning it does
  **zero** resource work — which is what makes read-only queries cheap.
- Every run path (fresh `Run`, resume, timer wake) obtains prepared resources
  for that run via the off-mailbox prep, then runs the loop (already spawned
  today). No path runs off stale baked resources.
- **System prompt → agent state.** Computed once at the first run (empty
  history) via the scanner, folded into `AgentState` (new
  `SystemPromptSet` event), reused forever after. Live workspace freshness is
  still served mid-run by the existing `skill` / `inspect_workspace` tools.
- **Usage → agent state.** Add `usage_total: Usage` to `AgentState`; fold
  `RunComplete` into it (currently a no-op arm).
- **No self-resume off baked resources.** `on_recovery_complete` does only
  resource-free work (re-arm timers). The *driver* decides to resume: a session
  resumes on the next user `Run`; a workflow agent resumes because its driver
  re-drives with freshly prepared resources. Same rule for both — the earlier
  "session vs workflow asymmetry" was incidental, not fundamental.

### Read path (history)

- `AgentActor` answers a `GetHistory { before?, after?, limit }` command from
  its **in-memory** state → `{ messages, has_more, tasks?, usage? }`.
  `tasks`/`usage` are populated **only on the tail call** (no `before`/`after`)
  — the initial window — and omitted on scroll-back, matching "return the task
  list on the initial page only". Cursor is a **message id**, actor-issued;
  callers never see a journal seq.
- `SessionActor` answers a progression-history command from its own journaled
  state → `{ progressions, has_more }`.
- The server `/history` handler *asks the actors*; it never reads a journal.
- **SSE becomes purely live** — id-less deltas plus coarse events streamed as
  they happen; the server-side journal replay is removed. Reconnect catch-up is
  a client re-query of history after its cursors (the client already dedups by
  message id).

### Timeline (UI merge, option "b" relaxed)

- Two independently-cursored streams: agent messages and session progressions.
- The **UI keeps both cursors and merges them chronologically by timestamp** —
  no strict cross-journal ordering is imposed by the backend.
- This needs timestamps the wire types lack today: add a `created_at_ms` to the
  message wire event and to progression events (neither `Message` nor the
  domain events carry one now).
- The history request carries a flag `include = both | agent`:
  - `both` — the initial/combined window (conversation + progressions).
  - `agent` — scroll-back of the conversation only (the high-volume stream);
    progressions are sparse and don't need re-paging on every scroll.

HTTP shape:

```
GET /api/sessions/:id/history?include=both|agent&before=<cursor>&limit=N
→ {
    agent:   { messages, has_more, tasks?, usage? },
    session: { progressions, has_more }   // present only when include=both
  }
```

## Phasing (one PR per phase, each green independently)

1. **Resource actors + observable prep + agent decoupling.** Non-journaling
   resource actors doing off-mailbox work + reporting progressions;
   `SessionActor` journals/streams/aggregates them; agent driven per-run with
   prepared resources; system-prompt-as-state; `usage_total`-as-state; no
   self-resume off baked resources. Touches the `workflow` crate. No new
   user-facing surface beyond the (additive) live progression events; covered
   by existing workflow + session tests plus new ones.
2. **History read-model.** `GetHistory` on `AgentActor` (cursors, tasks,
   usage); progression-history on `SessionActor`; timestamps added to the
   relevant wire events.
3. **Server read path + live-only SSE.** `/history` endpoint (`include` flag,
   per-stream cursors) served via the actors; SSE converted to purely-live;
   server-side journal replay removed.
4. **Frontend two-cursor windowed timeline.** Windowed initial load
   (`include=both`), scroll-up load-more (`include=agent`), chronological merge
   of the two streams, live SSE merge, scroll-position preservation on prepend.

## Testing / gate

Standard per phase: co-located Rust unit tests (state folds, command handlers,
the `MockVendor`-style resource-actor progressions), integration tests in
`tests/` (prep off-mailbox; recovery; history queries), Playwright e2e for the
long-history windowing and progression audit, and the full
`fmt`/`clippy`/`test`/`typecheck`/e2e gate to green before each PR.

## Implementation status (2026-07-21)

- **Agent resource decoupling** — done (#37): `RunResources`/`PreparedRun`,
  `FixedRunResources` (workflow), `SessionRunResources` (session prep off the
  mailbox), `usage_total` in `AgentState`.
- **History pagination** — done (#38): `AgentActor::GetHistory`, `/history`
  endpoint via a live-or-transient read-only agent, live-only SSE (`?live=1`),
  frontend windowed load + scroll-back.
- **Observable progressions** — first cut (this PR): resource-preparation stages
  (`provisioning_runtime`, `scanning_workspace`, `connecting_tools`, `ready`)
  broadcast **live** as `SessionEvent::Progressed` and shown in the composer
  area while a turn spins up. Emitted directly onto the frame stream by
  `ensure_runtime` (mailbox) and `SessionRunResources::ensure` (off-mailbox).
- **Deferred** (next): journaling progressions for durable audit + the
  two-stream (messages ⨁ progressions) history merge with per-stream cursors and
  message/progression timestamps; extracting standalone `RuntimeActor`/`McpActor`
  (the current cut keeps runtime/MCP lifecycle on the session but already moves
  the heavy prep off the mailbox and surfaces its progress).

## Out of scope (deliberate)

- Persisting resource-actor live state (their ephemerality is the point).
- Strict cross-journal chronological ordering on the backend (UI merges).
- Snapshot-compaction of the agent journal (principle 2 keeps that a future,
  caller-transparent change).
- Auth / multi-user.
