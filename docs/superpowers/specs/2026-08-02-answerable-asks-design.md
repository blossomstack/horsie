# Answerable asks — design

Date: 2026-08-02
Status: approved, ready for an implementation plan

## The report

A live session (`74145a86-…`, deepseek-v4-flash) showed two questions in its
transcript. Neither could be answered: the first was inert, and after the server
idle-offloaded the session, the second stopped responding to clicks too.

## Root causes

Four independent defects, found by reading that session's journal
(`GET /api/sessions/74145a86-…/history`, 53 messages) back into the code.

### 1. A rejected handoff is journaled as a live question

The model called `task_list` and `ask_user` in one assistant message (msg 50).
`Agent::validate_handoff` (agentcore/src/agent.rs) rejects a handoff issued
alongside any other tool call, so the turn was thrown away and the model was
nudged; it re-issued the ask alone (msg 51). There is no user message between the
two, which is how we know it was one run, not two turns.

The rejection pushes a `tool_result` for every call in the turn onto
`self.history` — **and emits nothing**. The caller's journal is built from emitted
events, so it kept two tool calls that never got a result. A tool call with no
result is exactly the shape of a question still waiting on the user, so the UI
rendered the abandoned ask as a live card.

### 2. Offload throws away the session's status

`SessionSupervisor::forget` clears `self.status`, and `SessionSupervisorCommand::Get`
reads only that map. So `GET /api/sessions/:id` returns no `status` for an
offloaded session — confirmed live: the response carried `pendingQuestion` but no
`status`. The web client gates answerability on `status === AwaitingInput`
(SessionView.tsx), so *every* card went read-only. Reloading doesn't help:
`on_recovery_complete` only reports a status when the session was `Running`, so a
durable `AwaitingInput` is never republished. Same hole after any server restart.

### 3. The ask's identity never reaches the client

`SessionState.pending_ask` holds the ask's tool-call id durably, and
`get_session` already reads it. Only the question *text* is on the wire, so the
client reverse-engineers which card is answerable by scanning the transcript
(`findPendingAsk`) under a "only the newest ask can be pending" heuristic.

### 4. An open tab churns the server

The frame broadcast channel is created inside `SessionActor::new`, so its sender
dies with the actor. Offload closes every SSE stream on that session
(`RecvError::Closed`); the browser's `EventSource` reconnects; `Subscribe` calls
`ensure_loaded`; the session reloads and resets `last_activity`; 180s later it
offloads again. A tab left open overnight cycles ~160 times, replaying the session
and agent journals each time, purely to unload again.

## Design

### Fix 1 — a park is not a conclusion (agentcore)

`validate_handoff` splits by handoff kind:

- **Optional handoff** (`ask_user`) alongside other tool calls is **legal**. The
  siblings execute concurrently as usual, their results are emitted and journaled,
  and only then does the run park. Legal because the run resumes on this same
  history once the answer arrives, so the siblings' results are ordinary work the
  model will read.
- **Forced handoff** (`conclude`) alongside other calls stays rejected: it ends the
  run for good, so nobody would ever read a sibling's result.
- Every rejection now **emits `ToolComplete`** for each call in the turn, so the
  journal matches the wire history and a rejected ask renders as a dead card.

Handoff tools are never executed by the toolbox, so the handoff calls are excluded
from the batch.

`AgentResult::Handoff(HandoffOutput)` carries **all** parked calls:

```rust
pub struct HandoffOutput {
    pub tool_name: String,
    pub calls: Vec<HandoffCall>,   // { tool_call_id, data }
}
```

For a forced handoff `calls.len() == 1` always. This replaces workflow's
`find_tool_call_id` event scan, which could only ever recover one id.

### Fix 2 — the actor answers for its own session (server)

Status is fully journaled: `TurnBegan` → Running, `TurnEnded`/`TurnStopped`/
`TurnInterrupted` → Idle, `AskRecorded` → AwaitingInput, `TurnFailed { error }` →
Failed, `SessionFailed { reason }` → Unrecoverable. `SessionActor::apply_event`
reconstructs all of it, reason included, when the framework replays the journal on
load. Nothing else needs to read that journal.

Today something else does. `get_session` calls `fold_session_state`
(server/src/sessions/events.rs) to replay the journal *itself*, because
`SessionSupervisorCommand::Get` only reads the supervisor's in-memory `status`
cache and an unloaded session isn't in it. That bypass is the actual defect: a
second reader of state the actor owns, which answers `pending_question` and
`inbox` but was never extended to `status`.

So:

- `Get` goes through `ensure_loaded` and asks the actor for a snapshot of its
  state — status, pending asks, pending question, inbox, last error — exactly as
  `History` and `UsageStats` already do. Opening a session page recovers its actor,
  which is the expected behaviour anyway.
- `fold_session_state` and its only caller are deleted. It is the last journal
  reader outside the actor.
- `on_recovery_complete` reports the recovered status to the supervisor cache and
  the frame channel, so a page open before the load sees a `StatusChanged` rather
  than silence.
- A journaled `Running` found at load is a dead turn; `ReconcileInterrupted`
  already resolves it to `Idle` before anyone reads it.

`list_sessions` keeps reading the cache alone and loads nothing: it is the one
caller where the cache earns its keep, since asking each actor would load every
session in the list. Unloaded sessions therefore still badge as "—" there, which
is honest — nobody has asked for them.

### Fix 3 — the status carries what you need to act on it

Server-side `SessionStatus` is already a data-carrying enum (`Failed { reason }`).
`AwaitingInput` joins them:

```rust
AwaitingInput { asks: Vec<PendingAsk> }   // { tool_call_id, question }
```

`SessionState.pending_ask: Option<String>` becomes `pending_asks: Vec<PendingAsk>`.
**The `AskRecorded` event shape does not change** — one is journaled per ask and the
fold appends. Old journals stay readable, which is the constraint that matters:
renaming persisted variants is what took the homelab down in #101.

The wire projects the payload onto both `SessionDetail` and the SSE
`StatusChanged` frame, so the client learns the pending asks live *and* on reload
from one shape. No query invalidation. `findPendingAsk` and its newest-ask
heuristic are deleted — the server names the answerable calls.

### Fix 4 — the frame channel outlives the actor (server)

The supervisor owns `BTreeMap<SessionId, broadcast::Sender<SessionFrame>>` and
hands a clone to each `SessionActor` it spawns. `forget` keeps an entry while
`receiver_count() > 0` and drops it otherwise; `Delete` removes it.

Offloading a watched session then leaves its SSE streams connected and silent —
correct, since nothing can happen while unloaded — and frames resume when anything
reloads it. `Subscribe` is answered from the registry without loading the session:
a broadcast channel is transport, not state the actor owns, and no journal is read
to hand one out.

### Fix 5 — answering is atomic (server + web)

A dedicated endpoint takes answers for **every** pending ask:

```
POST /api/sessions/:id/answers   { answers: [{ toolCallId, text }, …] }
```

An answer set that does not cover the pending set exactly is a 400 — no partial
state, no half-answered turn. The session injects the answers as one input and
resumes; the agent's history then holds a result for every parked call, so no
provider ever sees a dangling `tool_use`.

`AgentInput::ToolResult` carries one id, so it gains
`ToolResults(Vec<ToolResultInput>)`, whose `to_message()` builds one Tool-role
message with N `ToolResult` parts. **Verification required:** the OpenAI wire must
split that into N `tool` messages; Anthropic takes it as one user message with N
`tool_result` blocks. `repair_unanswered_tool_calls_except` takes a set of ids
rather than one.

**A plain composer message abandons the pending asks.** Each gets a journaled
result — an error carrying "not answered" — and the message starts a fresh turn:
"never mind, do this instead". This is a behaviour change: today a queued message
*is* the answer via `drain`. It is what makes the all-or-nothing rule enforceable,
since answers then have exactly one path.

Both paths collapse into one agent command, which replaces `Run` and
`InjectToolResult`:

```rust
AgentCommand::Resume { results: Vec<ToolResultInput>, message: Option<String> }
```

- `message: None` → the run's input is `AgentInput::ToolResults(results)`
  (answering; `results` must be non-empty).
- `message: Some(text)` → `results` are journaled onto the history first, then the
  run's input is the user message (abandoning, or a plain turn with `results`
  empty).

`TurnBegan` clears all pending asks on fold, and gains
`#[serde(default)] answered: Vec<String>` for provenance; the existing
`answering: Option<String>` field stays readable for old journals.

### Web

Pending asks render as a group. Each card collects its own selection and free
text; one submit sends them together and stays disabled until every ask has an
answer. An errored ask (a rejected handoff) renders as superseded rather than
putting the rejection text where an answer goes.

### Fix 6 — the journal is unreachable outside its actor (server)

Reading a journal from outside the owning actor has now caused this class of bug
more than once, so the invariant gets enforced by the compiler rather than by
review. Three bypasses exist today, all in `server/src/http`:

| site | call |
| --- | --- |
| handlers.rs:224 | `fold_session_state(&state.journal, uuid)` |
| sse.rs:92 | `journal_head_seq(&state.journal, sid)` |
| sse.rs:102,117 | `replay_session_events(&journal, sid, last)`, re-read per journaled frame |

All three are enabled by one line — `AppState.journal: Arc<dyn Journal>`
(http/mod.rs:47). The HTTP layer is handed the journal, so replaying it is one
call away and looks reasonable in review.

1. **Take the handle away.** `journal` leaves `AppState`; it is constructed in
   `http/mod.rs` and handed only to `spawn_root` and the actor deps. A handler then
   *cannot* replay, and the compile error a future author meets ("no field
   `journal`") points at adding a supervisor command, which is the right instinct.
   SSE's cursor and replay become actor-served commands, with
   `SessionCommand::History` as the precedent. The per-frame re-read becomes a
   command per journaled frame — which only happens while the actor is loaded,
   since something journaling means something is running.
2. **Make the read API unnameable.** `fold_session_state` and
   `replay_session_events` are `pub` free functions in `sessions::events`, which is
   what made them reachable; they become `pub(in crate::sessions)`, and the handle
   is wrapped in a `SessionJournal` newtype whose `replay`/`head_seq` are scoped the
   same way. Rust visibility is compile-time and total: `http` cannot name the
   method, so it cannot call it even holding a handle. This catches what step 1
   misses — a bypass written inside `crate::sessions` by something that isn't the
   actor.
3. **Backstop with a lint.** `clippy.toml` `disallowed-methods` on the journal's
   `replay`, with `#[expect(clippy::disallowed_methods)]` at each legitimate site.
   CI already runs `clippy --all-targets -- -D warnings`, so a future bypass can
   still be written — but not silently: it shows up in the diff as an explicit
   opt-out with its justification beside it.

What this does not prevent is someone re-adding `journal` to `AppState`. That is a
visible, arguable line in a diff, where `replay()` on a handle already in scope is
invisible; the asymmetry is the point.

## Testing

- agentcore: an optional handoff alongside other tools runs them and parks; a
  forced handoff alongside other tools is still rejected; a rejection records a
  result for every call in the turn; several asks in one turn park together.
- server: `Get` on an unloaded session loads its actor and reports the recovered
  status; `AwaitingInput` carrying ask ids
  on both `SessionDetail` and `StatusChanged`; a partial answer set is a 400; a
  full set resumes the turn; a composer message abandons pending asks; a frame
  channel survives offload with a live subscriber.
- web e2e: answer an ask after a reload with no status; answer a two-ask turn;
  a partial submit is refused by the UI.
- providers: a Tool-role message with N results maps correctly on both wires.
- SSE keeps replaying durable events with stable ids once its cursor and replay
  are actor commands, including a reconnect with `Last-Event-ID`.

## Out of scope

- Status on the session *list* (would load every session in the list).
- Changing idle offload itself — Fix 4 removes the reason to.
