# Server timestamps on journaled messages and events

Design for [#52](https://github.com/blossomstack/horsie/issues/52).

## Problem

Nothing in a session's journal records *when* anything happened. Messages carry
`id`, `role`, `parts` and no time; most `AgentDomainEvent` and
`SessionDomainEvent` variants carry no time either. Consequences:

- Turn and tool durations cannot be measured, so the UI cannot say "ran for 3m".
- A stuck-turn watchdog has nothing to compare against — it cannot know how long
  a tool call has gone unanswered.
- A journal pulled off the homelab for debugging cannot be laid out on a
  timeline; event order is known, elapsed time is not.

`SessionDomainEvent::MessageQueued` and `ProgressionEvent` already carry
`at_ms: u64` (unix-epoch milliseconds). This work finishes that pattern rather
than inventing a second one.

## Approach

Stamp the *event payloads*. Every timestamp lives inside the thing it describes,
so `apply_event` can fold it into state and every reader — the actor runtime,
the server's two direct journal readers, an operator reading `journal.jsonl` —
sees it without extra machinery.

A journal envelope (`{at_ms, event}` wrapped around every persisted event at the
`horsie-actor` layer) was considered and rejected. It is invisible to
`apply_event`, so it cannot supply `Message.created_at_ms` in `AgentState` —
which is the timestamp `/history` actually serves, since history is answered
from in-memory agent state and never from the journal. The envelope would have
replaced no per-event field, added a second source of time that can disagree
with the field beside it, and broken the on-disk format for every actor.

## Schema changes

### `models/fluorite/agent.fl`

```
struct Message {
    id: String,
    role: Role,
    parts: Vec<ContentPart>,
    /// Unix-epoch ms when the server finalized this message.
    created_at_ms: u64,
    /// Unix-epoch ms when the work that produced it began. Assistant messages
    /// only (the provider call's start); absent on user and tool messages.
    started_at_ms: Option<u64>,
}
```

`created_at_ms` is required, not optional. Every message the server produces has
one, and a required field makes the compiler enumerate all construction sites
instead of leaving silent nulls behind. `Message::user`, `Message::tool_result`
and `AgentInput::to_message` in `models/src/lib.rs` take the stamp as a
parameter rather than reading the clock themselves, so a caller that also
stamps an event uses one instant for both.

### `models/fluorite/events.fl`

```
struct ToolCompleteEvent { …, at_ms: u64 }
struct RunCompleteEvent  { …, at_ms: u64 }
```

The streaming events carry the stamp so the tool-result message agentcore holds
in memory and the one the actor folds from the journal are the same instant.
Reading the clock separately in each layer would let a replayed transcript
disagree with the live one about when a tool finished.

### `models/fluorite/session.fl`

```
struct ToolOutputEvent   { tool_call_id: String, output: String, is_error: bool, at_ms: u64 }
struct TurnCompletedEvent { iterations: u32, usage: Usage, at_ms: u64 }
```

`MessageEvent` is left alone: the `Message` it wraps carries its own stamps, and
a second timestamp on the envelope could disagree with it.

### `AgentDomainEvent` (`workflow/src/agent_actor.rs`)

- `ToolComplete` gains `at_ms: u64`. Required, because `apply_event`
  reconstructs the tool-result `Message` during replay — without a journaled
  stamp, recovery would re-stamp every historical tool result with replay time.
- `RunComplete` gains `at_ms: u64`, which feeds `TurnCompletedEvent`.
- `InputMessage` and `MessageComplete` need nothing: their `Message` carries the
  stamps.
- `RunCancelled`, `Parked`, `TimerArmed`, `TimerCancelled`, `TimerFired` and
  `TaskListChanged` gain `at_ms: u64` so an ops timeline has no blind spots.
  `RunCancelled` and `Parked` become struct variants.

### `SessionDomainEvent` (`server/src/sessions/session_actor.rs`)

Every variant gains `at_ms: u64` except `MessageQueued`, which already has one:
`TurnBegan`, `AskRecorded`, `TurnEnded`, `TurnFailed`, `TurnStopped`,
`TurnInterrupted`, `SessionFailed`, `UsageRecorded`, `SubAgentSpawned`,
`SubAgentRunning`, `SubAgentCompleted`. `TurnEnded`, `TurnStopped` and
`TurnInterrupted` become struct variants.

## Timestamp semantics

| Message | `created_at_ms` | `started_at_ms` |
|---|---|---|
| User | when the `InputMessage` event is created (turn start) | absent |
| Assistant | when the streamed message completed | when the provider call began |
| Tool result | when the tool finished | absent |

A user message's stamp is turn start, *not* accept time. Queued messages are
merged into a single turn message (`MERGE_SEPARATOR` in `session_actor.rs`), so
per-message accept times cannot survive into the transcript. The accept time is
already recorded, per message, in `SessionDomainEvent::MessageQueued.at_ms` and
surfaced on the inbox.

Every other `at_ms` means "when this event was created", stamped immediately
before it is handed to `CommandEffect::persist`.

## Stamping sites

- `agentcore/src/agent.rs` — the assistant `Message` literals (`agent.rs:369`,
  `558`, `634`) take `started_at_ms` from a wall-clock reading taken before the
  provider call and `created_at_ms` from one taken at completion. Tool-result
  and user messages pushed onto `self.history` stamp at construction.
- `workflow/src/agent_actor.rs` — `coarse_event` copies `at_ms` off the
  streaming `AgentEvent` onto its domain event rather than re-reading the clock.
  `InputMessage` events built by the actor stamp their `Message`. Synthetic
  repair messages (`repair_unanswered_tool_calls`, `missing_tool_results`)
  stamp at repair time.
- `server/src/sessions/session_actor.rs` — each `SessionDomainEvent` stamps at
  construction.

## `now_ms()` consolidation

Four wall-clock helpers exist today: `server/src/sessions/session_actor.rs:62`,
`server/src/http/handlers.rs:36`, `workflow/src/workflow_actor.rs:65`, and
`workflow/src/timers.rs::now_unix_ms`. Consolidate into
`horsie_models::now_ms()` — every crate involved already depends on
`horsie-models`, and it sits beside the `at_ms` fields it feeds. Delete the
four copies. `horsie-actor` is untouched: it stays domain-free and needs no
clock under this design.

No injectable clock. `server/src/sessions/clock.rs` exists for idle-offload
tests and is `Instant`-based (monotonic), which is the wrong type for a
wall-clock stamp; tests here assert ordering and presence, not exact values.

## Wire and SSE

All durable SSE events already flow through `replay_session_events` — live ones
arrive as a `SessionFrame::Journaled` wakeup that re-reads the journal for
stable sequence ids — so there is exactly one place where a journaled event
becomes a wire event, and it reads `at_ms` straight off the decoded domain
event. `wire_event` maps `ToolComplete.at_ms → ToolOutputEvent.at_ms` and
`RunComplete.at_ms → TurnCompletedEvent.at_ms`.

`HistoryPage.messages` needs no change: the timestamps ride on `Message`.

## Web UI

- `WorkGroup` summary row gains a duration suffix — "Thought and ran 3 tools ·
  1m 12s" — computed from the first and last message stamps in the group. While
  live, the row shows elapsed time for the running tool instead.
- User and assistant turn boundaries show a small absolute time.
- A shared `formatDuration` helper in `clients/web/src/lib/`.
- Regenerate `clients/ts` and `clients/web/src/generated` from the schemas; the
  CI drift job covers both.

## Testing

- `models` — `now_ms()` returns a plausible epoch millisecond.
- `agentcore` — a completed run's assistant message has `started_at_ms <=
  created_at_ms` and both are non-zero.
- `workflow` — folding a journal twice yields identical tool-result stamps
  (replay determinism); `coarse_event` stamps `ToolComplete` and `RunComplete`.
- `server` — `replay_session_events` surfaces `at_ms` on `ToolOutputEvent` and
  `TurnCompletedEvent`; a history page's messages carry `createdAtMs`.
- `clients/web` — `formatDuration` unit tests; an e2e assertion that a work
  group renders a duration.

## Compatibility

This breaks existing persisted state, in two ways.

`Message.created_at_ms` is required, so an old `AgentState` snapshot whose
messages lack the field fails to deserialize and `recover()` surfaces an error —
the failure mode that bricked the homelab in #101. Separately, promoting unit
variants (`RunCancelled`, `Parked`, `TurnEnded`, `TurnStopped`,
`TurnInterrupted`) to struct variants changes their serialized shape from
`"RunCancelled"` to `{"RunCancelled":{"at_ms":…}}`, so old journal lines for
those variants no longer decode either.

Existing homelab sessions must therefore be wiped on deploy
(`/data/data/server/actors` in volume `horsie_horsie-data`). This is an accepted
cost of a clean contract, decided explicitly. Note that the snapshot break alone
already forces the wipe, so the unit-variant change adds nothing to the cost.

## Out of scope

- The stuck-turn watchdog itself. This work makes it possible; it does not build
  it.
- Explicit measured per-tool execution duration. Tool time is derivable from
  adjacent message stamps; an exact figure for parallel tool calls would need
  the runtime to report its own timing.
- Backfilling timestamps onto existing transcripts.
