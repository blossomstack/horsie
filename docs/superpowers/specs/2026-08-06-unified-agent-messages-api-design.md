# One agent log, one endpoint

## The problem

Reading a session's messages takes three server calls today: `GET /sessions/{id}/agents/{aid}/history` for the transcript window, `GET /sessions/{id}/agents/{aid}/events` for that agent's live frames, and `GET /sessions/{id}/events` for session-scoped current values. A fourth read, `GET /sessions/{id}`, supplies status, inbox and `lastError` to the render path.

Every one of those is a separate source for some fact, and separate sources cannot be ordered against each other. PR #246 is the third attempt to fix one instance of that — the queued-message list, which had a snapshot from the detail endpoint racing an `InboxChanged` frame — and it fixes it by removing one of the two sources. The same shape remains for status (`StatusChanged` frame vs the session document), for the task list (`TaskListChanged` frame vs the agent document, guarded by a `tasksLive` latch), and for the last turn's error (`Error` frame vs `lastError`, guarded by an `errorLive` latch). Each guard manages the ambiguity rather than removing it.

The client is where this accumulates. `useSessionStream` carries `seed-queue`, `sawLiveInbox`, `seed-tasks`, `tasksLive`, `seed-error`, `errorLive`, `needsResync` and a backfill effect, a `hookEntryIds` dedup ledger and a `seen` set — all of them reconciliation between sources that should never have been two.

## What this replaces it with

One endpoint. One log per agent. The agent actor is its only writer, so its order is deterministic by construction: one actor, one mailbox, one fold, and replay reproduces exactly what it produced live. There is no merge anywhere in the read path.

Session-scoped facts reach the client because the session actor tells the target agent to append them. The session keeps every responsibility it has today; only notification is added, and it flows one way — session to agent, never back.

## Locked decisions

1. **The unified API reads one agent's log and never merges.** `aid` names the log; it is not a filter over a shared one.
2. **Two classes on one stream.** Durable entries carry a cursor and replay. Deltas carry no durable cursor and are never replayed — the finalized message supersedes them.
3. **The cursor is a monotonic per-agent ordinal**, assigned in the fold and stored on the entry.
4. **Deltas are numbered within the entry they follow**, giving a two-part cursor `(entry_seq, delta_seq)`. A stale delta number is arithmetically detectable, which a flat counter could not manage.
5. **Deltas are never journaled.** In-memory only, cleared when the message completes.
6. **The agent owns its inbox and decides when a queued message becomes a turn.** The session's remaining involvement is a provisioning gate.
7. **No broadcast channels.** A `watch` carries position; the data is read from the log.
8. **Existing transcripts are wiped, not migrated.**
9. **The task list is in the log *and* on the agent document**, under the constraint in "Two sources, one rule" below.

## The endpoint

```
GET /api/sessions/{id}/messages?aid={agent_id}
```

`aid` defaults to `main`.

| Deleted | |
|---|---|
| `GET /sessions/{id}/agents/{aid}/history` | folded in |
| `GET /sessions/{id}/agents/{aid}/events` | folded in |
| `GET /sessions/{id}/events` | folded in — session events now reach the client through the agent's log |

`GET /sessions/{id}` survives as a settings and metadata read (name, spec, group). It leaves the render path entirely: it is no longer a source of status, inbox, or `lastError`. `GET /api/events`, the global session-list feed, is untouched — different scope, different consumer.

`GET /sessions/{id}/agents/{aid}` survives, carrying `usage_total`, `last_turn_usage`, `context_tokens` and the task list.

### Three request forms

| Form | Meaning |
|---|---|
| `?aid=main` | stream: everything from the start, then live |
| `?aid=main&after=<cursor>` | stream: everything after the cursor, then live |
| `?aid=main&max=100&before=<seq>` | page: the 100 entries before `seq`; returns and closes |

The presence of `before` selects the page form. No `Accept` negotiation and no mode flag.

### Stream framing

SSE, one entry per event, using the browser's native resumption:

```
id: 99
data: {"seq":99,"atMs":1234,"body":{"type":"Llm","value":{...}}}

id: 99.50
data: {"entrySeq":99,"deltaSeq":50,"text":"...chunk..."}
```

`id:` is the cursor, so `Last-Event-ID` on reconnect *is* `after=`. A plain integer names an entry; `N.k` names the k-th delta after entry N.

### Page response

```json
{ "entries": [ ... ] }
```

No `has_more`. Fewer than `max` entries means there are no more. The flag was a second way to say the same thing, and `has_more_after` on a forward page is already a source of bugs in the current reducer.

### Errors

| Case | Answer |
|---|---|
| unknown session or `aid` | 404 |
| `after` beyond the tail | stream: nothing yet, park on the watch. Page: empty. |
| `before` names an unknown seq | empty page |
| the agent stops mid-stream | close the SSE; the browser reconnects and gets a fresh read |

## The log

Replaces `AgentState.history: Vec<HistoryEntry>`.

```rust
struct AgentLogEntry {
    seq: u64,
    at_ms: u64,
    body: AgentLogBody,
}

#[type_tag = "type"]
enum AgentLogBody {
    Llm(Message),
    Hook(HookEntry),
    Lifecycle(LifecycleEvent),
}

#[type_tag = "kind"]
enum LifecycleEvent {
    Provisioning { stage: String, detail: Option<String> },
    MessageQueued { id: String, text: String },
    TurnBegan { consumed: Vec<String>, answered: Vec<String> },
    TurnEnded { outcome: TurnOutcome },   // Ended | Failed(String) | Stopped | Interrupted
    AskRecorded { tool_call_id: Option<String>, question: String },
    SubAgent { id: Uuid, label: String, status: String },
    Step { index: u32, status: String },
    TaskList { tasks: Vec<TaskRecord> },
    SessionFailed { reason: String },
}
```

Nesting a union inside a union arm is already proven in this codebase: `HistoryEntry::Hook(HookEntry)` holds `HookEntry.record: HookRecord`, which holds `HookRecord.action: HookAction`.

**Why `Lifecycle` is one arm rather than eight.** `prompt_messages()` is a `filter_map` over the union, and a three-arm match where `Lifecycle => None` covers every lifecycle variant that will ever exist. Flattening the variants into `AgentLogBody` would make provider isolation a per-variant obligation that a future addition could forget. It also stops the top-level wire union changing shape every time lifecycle grows.

There is no common-field wrapper struct in the style of `HookRecord`: `at_ms` lives on `AgentLogEntry` and the agent is implicit in the log, so nothing universally true is left to factor out.

**`seq` is stored, not implied by index.** The history is never front-trimmed today, so index would work — but storing it keeps front-trimming available for context management later, and lets `before`/`after` resolve by binary search instead of the current `position(id)` linear scan.

**`id` and `seq` are different things and both stay.** `id` identifies (tool-call correlation, client dedup); `seq` orders. Splitting them is what removes the scan.

**Assignment happens in the fold**, from a `next_seq` counter in agent state — the same mechanism that makes `hook:{n}` deterministic today. Only entries consume numbers; agent events that are not entries (`TimerArmed`, `Parked`, `RunCancelled`, `RunComplete`) do not.

**Durability contract.** This field is in the snapshot. Every field carries `#[serde(default)]`; add optional fields, never rename or repurpose one. The 2026-08-02 outage was a renamed persisted variant taking down `recover()` for every session.

### Deltas

```rust
deltas: Vec<String>,   // chunks since the last entry; cleared when a message completes
```

In memory only. Not serialised, not journaled, not part of the fold. The sub-sequence is the index.

Journaling them was considered and rejected on cost: every `persist` is a write transaction (`begin_write` → `UPDATE journal_logs` → INSERT → commit), so it would put a disk commit on the critical path of every token and contend with every other write in the session. A 1000-token response is 500–1000 delta events; a 100-turn session goes from roughly 300 rows to 50–100k. A delta's useful life ends when the finished message lands, under a second later. And "replay from the beginning" would replay every token of every past turn, so the client would need skip logic regardless.

### Cursor resolution

| Client sends | Server state | Answer |
|---|---|---|
| `after=(S, d)`, `S < tail` | client is behind | entries from `S`, **no deltas**, `delta_seq` resets to 0 |
| `after=(S, d)`, `S == tail`, `d <= len(deltas)` | caught up | deltas after `d` |
| `after=(S, d)`, `S == tail`, `d > len(deltas)` | run restarted | all deltas from 1, with a reset marker |

No run id is needed: deltas are always "after the tail entry", so two numbers are unambiguous.

The third row is the trap this scheme exists to close. Entry 99 is the last saved thing; the agent streams 50 deltas; the client's cursor is `(99, 50)`; the process dies. On restart entry 99 is still the last saved thing, a new run starts, and it has emitted 20 deltas. The client asks for everything after `(99, 50)`, and `50 > 20` is impossible unless the run restarted — so the mismatch is arithmetic. A single flat counter reissuing 100–150 would look identical to the originals and nothing could notice.

The first row is the adaptive skip: "this client is not caught up enough for live typing to mean anything" falls out of the same comparison.

## Which session events go to which agent

| Session event | Appended to |
|---|---|
| `Provisioning{Started,Succeeded,Failed}` | main |
| `MessageQueued` | the agent the message was addressed to (`aid`) |
| `TurnBegan`, `TurnEnded/Failed/Stopped/Interrupted` | the agent whose turn it is; the last four collapse into one `TurnEnded` entry carrying the outcome |
| `AskRecorded` | the asking agent |
| `SubAgentSpawned/Completed/Failed` | the **parent** agent |
| `Step{Started,Concluded,Failed,Cancelled}` | the step's agent |
| `SessionFailed` | main |
| `UsageRecorded`, `SubAgentRunning`, `SubAgentNotified` | nothing — not viewer-facing |

## The write path

```
POST /api/sessions/{id}/messages?aid={agent_id}
```

1. Supervisor `ensure_loaded(session)` → session actor.
2. Session `resolve_agent(aid)` — spawns it if cold, which is one journal replay and no vendor call.
3. Session tells the agent `Enqueue { id, text }`.
4. Agent journals `MessageQueued`, appends the log entry, replies via `CommandEffect::PersistAndAck`.
5. HTTP returns the message id **and its `seq`**, so the caller can stream from exactly there.

`PersistAndAck` already means "reply only after the durable write", so "persisted before the API returns" needs no new machinery. Agent construction is `Arc` clones plus `AgentParams::from_def` — no vendor call, no I/O — and `ctx.spawn` returns immediately while recovery runs before the mailbox drains, so a command sent right after is handled after replay rather than racing it.

**The agent decides when a queued message becomes a turn.** It already knows whether it is running and whether it is parked on an ask, because asks are per-agent. The only thing it does not know is whether the session's runtime exists.

**The provisioning gate is in-memory, seeded at spawn.** The session constructs every agent — main at recovery, subagents on demand — so it passes current readiness in the construction params and later pushes `Provisioning{Succeeded,Failed}` as a lifecycle notification. Nothing about the gate needs to be durable: an agent that does not exist cannot be holding a turn, and one that is respawned is re-seeded by the session that spawned it.

**Recovery still drains nothing.** A queued message waits for the user, exactly as today.

Two neighbouring endpoints take `aid` for consistency:

- `POST /sessions/{id}/answers?aid=` — asks belong to the agent that asked.
- `POST /sessions/{id}/stop` stays session-scoped and fans out. Per-agent stop is tracked separately in #247.

## Backpressure

There is no overflow path, because nothing is pushed at a reader.

The agent publishes its current position — `(tail_seq, delta_count)` — into a `tokio::sync::watch`. Each connection's task awaits a change, then asks the actor for everything between its cursor and the new position. A `watch` holds only the latest value and overwrites it, so a slow reader cannot fall behind: it sees a bigger jump when it next looks. The data never travels through the channel; only the notification does.

This removes `FRAME_BROADCAST_CAPACITY`, `STREAM_BUFFER`, `MAX_BACKFILL_PAGES` and `BACKFILL_LIMIT` — four tuning constants with no successor — along with `RecvError::Lagged` handling and the `Resync` frame.

The user-visible consequence of a slow client is that text may appear in jumps rather than smoothly. Nothing is lost or left stale. That is a strict improvement on today, where an overflowed session stream is silently skipped (`continue` in sse.rs) and leaves the status badge or a queued bubble frozen at a stale value until some unrelated change happens.

## Two sources, made comparable

The task list is in the log **and** on the agent document, and a UI component reads it from both today. That is deliberate and it is safe, but only because of one addition:

> **Every agent-document read is stamped with the log position it reflects.**

```
GET /sessions/{id}/agents/{aid}  ->  { tasks, usage_total, ..., as_of_seq: 200 }
```

A consumer records, per value, the seq it last set that value from. An update applies only if its seq is greater — whether it came from the document or from the log. Both sources land in one order, so "which is fresher" is arithmetic rather than a guess.

This is what today's `tasksLive` latch cannot do. A boolean says "a live frame has arrived, so ignore documents forever"; it cannot distinguish a document that is *ahead* of the fold from one that is behind. That limitation is why `Resync` has to reach in and set `tasksLive` back to false — a lost frame means the document is fresher again, and the latch has no way to express that. `errorLive` and the deleted `sawLiveInbox` are the same shape, and `sawLiveInbox` is precisely what PR #246 could not make work: a REST read and a broadcast frame have no ordering relationship, so no guard over them can be correct.

One number on the wire replaces three latches and their release hacks. The same stamp makes any future current value on that document safe to read alongside the log, so this is a rule rather than a special case for tasks.

## What deletes

**Server**

| Gone | |
|---|---|
| `SessionFrame` broadcast and the supervisor's `frames` map | no channel to own |
| `AgentFrame` broadcast and `agent_frames` map | one `watch` per agent |
| `FRAME_BROADCAST_CAPACITY`, `STREAM_BUFFER`, `MAX_BACKFILL_PAGES`, `BACKFILL_LIMIT` | no successor |
| `Resync` frame and its handling | overflow cannot happen |
| `SessionCommand::PublishInbox`, `SessionSupervisorCommand::PublishInbox` | PR #246 in its entirety |
| `SessionSupervisorCommand::Subscribe` / `SubscribeAgent` | one read command |
| the SSE reconnect backfill loop in `agent_events` | `after=` is the only path |

**Client — `useSessionStream`**

Gone outright: `seed-queue`, `sawLiveInbox`, `needsResync` and its backfill effect, `hookEntryIds` (entries cannot arrive twice under a monotonic cursor), the `seen` set in `applyHistory`, `liveStatus`/`statusSeq`/`statusReason`/`livePendingAsks`, `progression`, and the `useSession` read in the render path. Two `EventSource` connections become one.

Replaced rather than deleted: `tasksLive` and `errorLive` are latches; the document read still exists but is now reconciled by comparing `as_of_seq` against the seq each value was last set from. The guard survives as arithmetic instead of a boolean, and the `Resync`-releases-`tasksLive` hack goes with the latch.

`optimistic` stays — it is genuine local echo — but its kill signal becomes definite: the POST returns the `seq`, and the echo dies when that seq arrives. No more reconciling "acked" against "queued".

## Costs and risks

**The client owns a fold that must match the server's.** Status from turn events, inbox from `MessageQueued`/`TurnBegan`, tasks from `TaskList`. This is real duplication and it can drift. Mitigation is shared fixtures: the same log, asserted to produce the same status on both sides. The surface is smaller than what it replaces, but it is not free.

**Existing transcripts are wiped.** Renaming the state field means serde defaults the new one to empty, which is the safe failure — recovery survives — but every existing session loses its history. Accepted deliberately in preference to a migration path that would be thrown away.

**The session actor gains a notification obligation.** Every viewer-facing session event must be routed to the right agent, and forgetting one means a fact that silently never reaches the client. The routing table above is the contract; it needs a test that asserts every viewer-facing `SessionDomainEvent` variant has a destination.

## Testing

- **Determinism** — recover an actor twice from the same journal, assert byte-identical `(seq, body)` sequences. This is the property the whole design rests on and it should be a test, not an argument.
- **Cursor** — one case per row of the resolution table.
- **Crash and reuse** — entry 99, 50 deltas, restart, new run emits 20; assert `(99, 50)` is answered with a reset rather than silence.
- **Fold parity** — one log fixture; server fold and client fold asserted equal.
- **Routing completeness** — every viewer-facing `SessionDomainEvent` variant reaches an agent log.
- **E2E** — `e-progress-ux` stays. It is what surfaced #246 and it is the honest regression net.

## Out of scope

- Auto-restart of sessions on server boot. Additive, and not coupled to this.
- Per-agent stop — #247.
- Non-blocking session create and the workspace scan on the first turn — #248.
- The `Transcript` / `liveTurnIndex` rendering questions from PR #246. Downstream of this, and entangling them would make both harder to review.
