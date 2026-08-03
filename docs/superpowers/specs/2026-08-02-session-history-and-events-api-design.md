# Session history and live-events API: state-sourced, agent-scoped

**Status:** design approved, not yet implemented
**Date:** 2026-08-02
**Scope:** the HTTP/SSE surface for reading a session's transcript and watching it live
**Unblocks:** SQLite-backed journaling (separate spec) — that work needs `recover()` to be the only journal reader

## Problem

The session read APIs mix three unrelated concerns into one union type and two endpoints, and the live path reaches into the durable journal on every event. Concretely:

**The stream re-reads the whole journal on every event.** `SessionFrame::Journaled` is a contentless doorbell. On each one, `sse.rs:143` calls `events_of(last)`, which asks supervisor → session → agent, and `AgentActor::replay_journal` (`agent_actor.rs:485`) opens the journal and decodes **from sequence 0**, discarding everything at or below the cursor. One turn with *k* durable events on an *n*-event transcript costs *k* full-transcript reads, per connected client. This is a live-path cost, not a reconnect cost.

**`live=1` does not do what it claims.** The web client requests `live: true` (`useSessionStream.ts:458`), but the server still stamps durable events with `id:`, so the browser records `lastEventId` and sends `Last-Event-ID` on its automatic reconnect. The condition at `sse.rs:99` then takes the `else` branch and full-journal-replays. The "state-based client" silently reverts to journal replay on every network blip.

**The same append is on the wire twice, in two vocabularies.** `/history` returns `Vec<Message>`, where a tool result is a `ContentPart::ToolResult` inside a message. SSE sends `Message` events *and separate* `ToolResult` events. The web reducer consequently keeps `byId` **and** a parallel `toolResults` map and re-merges them at render — reimplementing a fold the actor has already done.

**`HistoryPage` means two different things.** `tasks` and `usage` are populated only when `before` is absent (`agent_actor.rs:370-376`). Same type, two contracts. That `usage` is a `UsageView` while `/usage` returns `SessionUsageStats` — two shapes for one fact.

**Three cursor spaces on one resource.** `/history` pages backwards by message id; SSE resumes forwards by journal sequence; `agent_id` is a third axis the stream does not have at all, so subagent work is invisible live.

**The journal-sequence cursor leaks an internal detail.** Journal sequence numbers are an event-sourcing implementation detail. Exposing them as the SSE id and the CLI's resume cursor is what forces `AgentParams::compact_on_pause()` (`agent_actor.rs:61-65`) to disable compaction for interactive sessions — its comment says so outright: "SSE cursors are journal sequence numbers and must stay stable." The hack is a symptom of the leak.

## The structural fact this design rests on

`AgentActor::apply_event` (`agent_actor.rs:995-1010`) shows that `InputMessage`, `MessageComplete`, and `ToolComplete` each do exactly one thing: **push one message onto `state.messages`**. Nothing mutates or removes an earlier entry. Tool results are appends too — `Message::tool_result` (`models/src/lib.rs:174`) assigns the deterministic id `result:{tool_call_id}`.

So the transcript is already an append-only log **in state**, and a message id is already a stable forward cursor. It needs no journal, it survives snapshotting because it *is* part of the state being snapshotted, and — since state is never truncated — it can never go stale.

`session_actor.rs:1477` already generalizes this to cold subagents: reading a subagent's history spawns its actor on demand so the read is served from recovered state. Recovery is the only journal access on that path.

## Design

Every durable thing a client observes falls into exactly one of three categories. Today's API mixes all three into one `SessionEvent` union.

| Category | What it is | Served by | Cursor |
|---|---|---|---|
| **Transcript appends** | `state.messages` entries | history endpoint + `Appended` frames | message id, forwards and backwards |
| **Current values** | task list, usage, status, pending asks, inbox, last error, agent tree, progression stage | document endpoints + `Changed` frames | none — read the current value |
| **Ephemeral** | text deltas, tool-start | live frames only | none — meaningless outside a live run |

Category 2 is the load-bearing insight. `TaskListEvent` already sends the whole list, `InboxChangedEvent` the whole queue, `StatusChangedEvent` the whole status. These were never log entries — they are value notifications, so they need no cursor and no backfill. On reconnect the client re-reads the document.

### Resource model

The addressable resource is an **agent within a session**, not the session.

```
GET /api/sessions/:id                          session document
GET /api/sessions/:id/events                   session frames
GET /api/sessions/:id/agents/:aid              agent document
GET /api/sessions/:id/agents/:aid/history      transcript appends, before= / after=
GET /api/sessions/:id/agents/:aid/events       agent frames
GET /api/events                                global session-list stream (unchanged)
```

`:aid` is `main` or a subagent uuid — the same vocabulary `/history?agent_id=` uses today.

Every resource now has the same shape: a document of current values, plus — where an append-only log exists — a history endpoint and a live stream sharing one cursor space.

### Scope split

`SessionDetail` today mixes two scopes. It splits on the agent boundary:

**Session document** — `id`, `name`, `created_at`, `model`, `vendor`, `repos`, `plugins`, `mcp_servers`, `memory_spaces`, `use_plugins`, `thinking_effort`, `status`, `last_error`, `pending_asks`, `inbox`, `usage_total`, and the agent tree.

**Agent document** — `tasks`, `usage`, `context_tokens`, `context_window`; and for a subagent additionally `label`, `task`, `parent`, `depth`, `status`, `output`, `error`.

`GET /api/sessions/:id/subagents` folds into the session document and is removed — the tree is a current value like any other. `SubAgentView.output` stops being deliberately absent; output and error are ordinary fields on the agent document.

`GET /api/sessions/:id/usage` splits along the same scope boundary rather than moving wholesale: today's `SessionUsageStats` bundles `session_total` (summed across every agent) with `main_agent` (that agent's own usage and context size). `session_total` is session-scoped, so it becomes `usage_total` on the session document; the per-agent half becomes `usage` + `context_tokens` on each agent document. `context_window` stays attached by the HTTP layer from model config, as it is today — it is not agent state. The endpoint itself is removed; its only caller already re-fetches on `StatusChanged`.

`pending_question` is dropped. It is a single-question duplicate of `pending_asks` kept for older clients, and this is a breaking wire change already.

### Frames

**Session stream** (`/sessions/:id/events`) — no SSE ids, all category 2 or 3:
`StatusChanged`, `InboxChanged`, `Progressed`, `Error`, `AgentTreeChanged`.

**Agent stream** (`/sessions/:id/agents/:aid/events`):
- `Appended { message }` — SSE `id: <message.id>`. Replaces both `Message` and `ToolResult`.
- `TurnCompleted`, `TaskListChanged` — category 2, no ids.
- `Delta`, `ToolStart` — ephemeral, no ids.
- `Resync` — new; emitted on `RecvError::Lagged`, telling the client to backfill via `history?after=<its cursor>`.

The `live` query parameter is removed. Streams are always live; backfill is the history endpoint's job.

### Resume

Only `Appended` frames carry `id:`. Because a stream maps to exactly one transcript, the browser's single `Last-Event-ID` per connection is sufficient and correct.

On reconnect the browser sends `Last-Event-ID: <message id>`; the server serves appends after it directly from `state.messages`, via the same code path as `after=`. The client re-reads the agent document on `onopen` to refresh current values. On lag, `Resync` triggers the same backfill.

**No journal reads on any of these paths.**

### Subagent visibility

`QuietEventSink` (`events.rs:62`) is replaced for subagents by a sink publishing to that subagent's own broadcast. This makes subagent work watchable live, and is correctly scoped: only a client actually viewing that subagent subscribes.

**Client rule: open streams for what is rendered, not for what exists.** A session view is 2 connections (session + main agent); opening a subagent panel makes 3. Eagerly opening a stream per tree node is forbidden — the default subagent limit is 8 and browsers cap HTTP/1.1 at ~6 connections per origin.

HTTP/2 lifts that cap entirely (~100+ multiplexed streams per connection), but browsers negotiate it **only via ALPN over TLS**; cleartext h2c is not implemented in any browser. horsie serves plain TCP today (`main.rs:221-228`, no rustls in `server/Cargo.toml`), so HTTP/2 is available behind a TLS-terminating reverse proxy (as on the homelab, via Caddy) and not to a bare `docker compose up` on `http://localhost:8080`. Native TLS is out of scope here and belongs with the auth work.

## What this removes

- `AgentCommand::ReplayEvents`, `AgentCommand::HeadSeq`, `AgentActor::replay_journal`
- `SessionCommand::Events`, `SessionSupervisorCommand::Events` / `HeadSeq`
- `SessionFrame::Journaled` (the doorbell)
- the `live` query parameter and all `Last-Event-ID` parsing in the server
- the `ToolResult` / `ToolOutputEvent` wire variant
- `HistoryPage`'s optional `tasks` / `usage`
- `GET /sessions/:id/subagents`, `GET /sessions/:id/usage`, `SessionDetail.pending_question`
- `AgentParams::compact_on_pause()` and its `interactive` coupling
- the web reducer's `toolResults` map

After this, **`recover()` (`actor/src/runtime.rs:107-131`) is the only code in the server that reads a journal.**

## Testing

- **Conformance of the two projections:** a session driven through a turn must yield the same transcript via `history` and via accumulated `Appended` frames. This is the invariant the current two-vocabulary design cannot state.
- **Resume:** disconnect mid-turn, reconnect with `Last-Event-ID`, assert no gap and no duplicate — and assert zero journal reads (a counting `Journal` wrapper in the testkit).
- **Lag:** force a `Lagged` broadcast and assert a `Resync` frame, then a successful `after=` backfill.
- **Cold subagent:** read a completed subagent's document and history after an offload; assert its actor is spawned on demand and the result matches pre-offload.
- **Web e2e:** the existing Playwright suite covers the session view; update for the new endpoints and add a subagent-panel stream case.

## Costs and consequences

- **Breaking wire change.** Web client and CLI both change. `session tail` moves to `history?after=` + the agent stream.
- **Reconnect needs an explicit document re-read** (`onopen`), where today the server papered over it with a full journal replay.
- **Broadcast carries payloads** instead of a doorbell — a message clone per subscriber, against today's *N* full journal replays. Cheaper, but new memory traffic.
- **Progressions stay live-only**, as decided in #37/#38/#39. The current stage moves into the session document; past stages remain unreplayable.
- **`AgentState` becomes a durability contract** once the SQLite work starts writing snapshots. It has never been serialized in production, so every field needs `#[serde(default)]` discipline — a renamed field would break `recover()` exactly as renamed event variants killed the supervisor on 2026-08-02.

## Sequencing

1. **This spec** — API redesign. Journal reads leave every path except `recover()`.
2. **SQLite journaling** (separate spec) — backend, snapshots, `replay` yielding `(seq, bytes)`, WAL. Only reachable cleanly once (1) lands, because today's cursor semantics pin the journal's sequence numbering.
