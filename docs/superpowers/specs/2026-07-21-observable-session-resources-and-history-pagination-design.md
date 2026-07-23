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
5. **Observable prep (live).** Resource progressions are streamed live so they
   appear during the wait. Journaling them for a durable audit is deliberately
   *not* done now (see "Context providers" below) — it can be added later without
   changing callers.
6. **Agents own their state.** Messages, tasks, usage, and the system prompt
   belong to the agent — which generalizes to multiple agents per session
   later, each owning its own.

## Architecture

### Context providers (plain code, not actors)

The seam between an `AgentActor` and the volatile resources one run needs is a
plain trait — **no resource actors, no audit journaling.** (An earlier draft
modelled the runtime and MCP as non-journaling child actors reporting
progressions to `SessionActor` for a durable audit. That was dropped: the audit
data is not needed now, and plain code is simpler. The trait keeps the seam, so
the actor/audit version remains a possible future change behind the same
interface.)

```rust
/// The per-run contexts an agent run executes within.
pub struct Contexts {
    pub provider: Arc<dyn LlmProvider>,
    pub toolbox: Arc<dyn Toolbox>,
    pub system_prompt: Option<String>,
}

#[async_trait]
pub trait ContextProvider: Send + Sync {
    async fn provide(&self) -> Result<Contexts, String>;
}
```

- **`ContextProvider::provide`** is called on the run's *spawned task* — never an
  actor mailbox — at the top of every run path (fresh input, resume, timer
  wake). It does the heavy, idempotent setup: rehydrate a suspended runtime,
  reconnect a dropped MCP, scan the workspace + SessionStart, compose the system
  prompt, build the toolbox. Cheap when everything is already live.
- **Implementations:**
  - `FixedContextProvider` (workflow) — runtime/toolbox provisioned once at
    spawn and reused; `provide` is a trivial clone, so a recovery self-resume
    gets them back unchanged.
  - `SessionContextProvider` (session) — resolves the provider live, scans the
    workspace, connects the enabled MCP servers, composes the system prompt; the
    sandbox round-trips that used to block the `SessionActor` mailbox now run on
    the run task.
  - `NoContextProvider` (session, read-only) — a transient history-reading agent
    that must never run; `provide` errors defensively.
- **Observable prep (live only).** `SessionContextProvider::provide` emits
  coarse progression stages (`scanning_workspace`, `connecting_tools`, `ready`)
  and `ensure_runtime` emits `provisioning_runtime`, straight onto the session's
  live frame broadcast (`SessionFrame::Progression`). Best-effort: no
  subscribers → dropped. Not journaled.
- **Extensibility.** A new resource is just more work inside a `ContextProvider`
  impl (or a new impl). The agent depends only on the `Contexts` handed to it;
  what composed them is owned by the session layer.

### Agent resource-decoupling

- `AgentActor` holds **no** provider/toolbox as identity. Its context
  (`AgentRuntimeContext`) is `event_sink`, `parent`, `session_id`, and a
  `context_provider` handle. Spawning it does **zero** resource work — which is
  what makes read-only history queries cheap.
- Every run path (fresh `Run`, resume, timer wake) obtains its `Contexts` via
  `context_provider.provide()` on the run's spawned task, then runs the loop. No
  path runs off stale baked resources.
- **Usage → agent state.** `usage_total: UsageTotal` on `AgentState`; fold
  `RunComplete` into it (previously a no-op arm).
- **System prompt.** Composed per run by `SessionContextProvider` from a live
  workspace scan (workflow agents carry a static prompt in their params). Live
  workspace freshness is still served mid-run by the `skill` /
  `inspect_workspace` tools.

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

1. **Context providers + observable prep + agent decoupling.** A plain
   `ContextProvider` trait producing per-run `Contexts` off-mailbox
   (`FixedContextProvider` for workflow, `SessionContextProvider` for sessions);
   live progression stages streamed from the session provider;
   `usage_total`-as-state. Touches the `workflow` crate. No new user-facing
   surface beyond the (additive) live progression events; covered by existing
   workflow + session tests plus new ones.
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
`ContextProvider` progressions over a `MockVendor`), integration tests in
`tests/` (prep off-mailbox; recovery; history queries), Playwright e2e for the
long-history windowing and live progressions, and the full
`fmt`/`clippy`/`test`/`typecheck`/e2e gate to green before each PR.

## Implementation status (2026-07-21)

- **Agent context decoupling** — done (#37): `ContextProvider`/`Contexts`,
  `FixedContextProvider` (workflow), `SessionContextProvider` (session prep off
  the mailbox), `NoContextProvider` (read-only), `usage_total` in `AgentState`.
  Plain code — resources are *not* modelled as actors (see "Context providers").
- **History pagination** — done (#38): `AgentActor::GetHistory`, `/history`
  endpoint via a live-or-transient read-only agent, live-only SSE (`?live=1`),
  frontend windowed load + scroll-back.
- **Observable progressions** — first cut (#39): resource-preparation stages
  (`provisioning_runtime`, `scanning_workspace`, `connecting_tools`, `ready`)
  broadcast **live** as `SessionEvent::Progressed` and shown in the composer
  area while a turn spins up. Emitted directly onto the frame stream by
  `ensure_runtime` (mailbox) and `SessionContextProvider::provide` (off-mailbox).
- **Deferred** (next): the two-stream (messages ⨁ progressions) history merge
  with per-stream cursors and message/progression timestamps. Explicitly *not*
  planned unless a need appears: journaling progressions for a durable audit and
  modelling runtime/MCP as actors — the plain `ContextProvider` seam can absorb
  either later without changing callers.

## Out of scope (deliberate)

- Modelling runtime/MCP as actors and journaling their progressions for a
  durable audit (dropped; the plain `ContextProvider` seam can absorb it later).
- Strict cross-journal chronological ordering on the backend (UI merges).
- Snapshot-compaction of the agent journal (principle 2 keeps that a future,
  caller-transparent change).
- Auth / multi-user.
