# Subagent Result Provenance — Design

Date: 2026-08-04
Status: Approved (design), pre-implementation

## Problem

A finished subagent's result reaches its parent as **text merged into the
parent's next user message**. `main_turn` in `server/src/sessions/orchestrator.rs`
joins the queued user text and every owed notification string with
`MERGE_SEPARATOR`, and `notification_text` prefixes each one with
`[subagent "<label>" completed]`.

That is correct on the wire and wrong in the web UI. The transcript renders raw
provider `Message`s and branches on `role` (`clients/web/src/components/Transcript.tsx`),
so a subagent result appears as a **user bubble** — visually indistinguishable
from something the person typed. A session that delegates work reads as if the
user kept pasting reports to themselves, and the actual conversation is buried.

The fix people reach for first — "render it as an assistant message" — is not
available. The stored message *is* the provider wire message; assistant-role
text would break Anthropic's role alternation and tell the model it said
something it did not.

## Goal

Give the client enough structure to render a subagent result the way it renders
tool traffic: a collapsed row in the assistant thread, expandable to the result,
never a user bubble. The provider wire must not change.

## Decisions (from brainstorming)

- **Standalone row where the result arrives.** Not folded back into the
  `spawn_agent` tool card. `spawn_agent` is async fire-and-forget, so the spawn
  and the result are turns apart; rendering the result at its arrival point is
  chronologically honest and needs no cross-turn correlation.
- **Results are always assistant-side.** In a turn that carries both typed text
  and owed results, the rows render *above* the user bubble, attached to the
  preceding assistant entry. Render order is deliberately decoupled from wire
  order (which stays text-then-results).
- **A row expands to the result text only.** No task, no drill-in to the
  subagent's own transcript. `GET /api/sessions/:id/history?agent_id=` already
  supports that drill-in; building a surface for it is a separate piece of work.
- **Rows carry a duration.** `SubAgentRecord` gains two timestamps. The same
  rationale `WorkGroup` already applies to tool groups: the one figure that says
  whether a collapsed row hides three seconds of work or three minutes.
- **Forward-only journal, accepted.** See Compatibility.

Non-goals: changing the `spawn_agent` tool card, a subagent transcript viewer,
surfacing the subagent tree anywhere else in the UI, migrating existing journals.

## Server

### New wire type

`models/fluorite/agent.fl`:

```
struct SubAgentResultPart {
    subagent_id: String,
    label: String,
    /// "completed" | "failed" — the SubAgentView.status vocabulary.
    status: String,
    /// Result body: output on success, error text on failure. Already capped
    /// at 50 KB by `truncate_result`, truncation marker included.
    text: String,
    spawned_at_ms: u64,
    ended_at_ms: u64,
}

union ContentPart { …existing…, SubAgentResult(SubAgentResultPart) }
```

`UserMessageInput` gains `subagent_results: Vec<SubAgentResultPart>`.

### The seam

`AgentInput::user_message()` keeps its signature and defaults the vec empty, so
the `Run { input: task }` that starts a subagent is untouched. `AgentInput::to_message()`
(`models/src/lib.rs`) appends one `ContentPart::SubAgentResult` per entry after
the text part — **and omits the text part entirely when the text is empty**.
That omission is load-bearing: an owed-only turn now carries no typed text, and
Anthropic rejects empty text blocks.

### Orchestrator

`TurnInput` (`server/src/sessions/orchestrator.rs`) gains
`subagent_results: Vec<SubAgentResultPart>`. Both producers stop joining
notification text into `message`:

- `main_turn` — `message` becomes the queued inbox text alone (`None` when the
  inbox is empty), and owed results go in the new field.
- `wake_owed_parents` — `message` becomes `None`; owed results go in the new
  field. A woken subagent parent is resumed with results only.

`SubAgentTree::owed_for` and `owed_by_sub_parent` return
`Vec<(Uuid, SubAgentResultPart)>` instead of `(Uuid, String)`. `notification_text`
survives unchanged but moves down the stack: it is now called by the *provider
serializers*, not by the orchestrator.

`AgentCommand::Resume` grows a matching `subagent_results` field, which
`workflow/src/agent_actor.rs` threads into `AgentInput::UserMessage`.

### Providers

`providers/anthropic/src/lib.rs` and `providers/openai/src/wire.rs` each grow one
match arm rendering the part through `notification_text(label, output, error)` —
byte-for-byte the string they send today. `server/src/wire_redact.rs` and the
remaining exhaustive `ContentPart` matches (`supervisor/src/history.rs`,
`agentcore/src/agent.rs`, `server/src/sessions/events.rs`,
`workflow/src/agent_actor.rs`) get arms too; clippy denies wildcard enum arms, so
the compiler enumerates the work.

### Durations

`SubAgentRecord` gains `spawned_at_ms` and `ended_at_ms`, both
`#[serde(default)]` so rows journaled before this ship load unchanged. Stamped in
`apply_spawned` and in `apply_completed`/`apply_failed` from the events' existing
`at_ms`. `SubAgentView` exposes both, keeping `GET /api/sessions/:id/subagents`
consistent with the transcript.

## Web

### Extraction

`useSessionStream.ts`: `RenderedMessage` gains `subagentResults: RenderedSubAgent[]`,
built by a new `subAgentResultsOf(parts)` beside `textOf`/`thinkingOf`/`toolCallsOf`.
`textOf` needs no change — it already filters to `Text` parts, so it stops
picking up notification text on its own.

### Grouping

`groupTurns` (`Transcript.tsx`) is where "always assistant-side" lives. Today a
`User` message unconditionally pushes a user turn. It becomes:

1. If the message carries subagent results, append a **synthetic
   `RenderedMessage`** — results only, empty text, no thinking, no tool calls —
   to the trailing assistant group, opening one if there is none.
2. Then, only if the typed text is non-empty, push the user turn.

An owed-only turn therefore renders **no user bubble at all**. A mixed turn
renders results above the bubble. The synthetic message is what keeps
`buildSegments` honest: with no text it cannot emit a stray text segment.

### Segments

`transcriptSegments.ts`: `WorkItem` gains `{ kind: "subagent"; result: RenderedSubAgent }`.
`buildSegments` pushes those into `work` ahead of the message's thinking items
and calls `extend(spawnedAtMs, endedAtMs)` so the group's duration span covers
them.

Making them `WorkItem`s is the point of the whole design: one result renders bare
(a single-item `WorkGroup` has no chrome), several flushing together collapse
into one summary row, and the existing duration and expand/collapse behaviour
applies untouched. `summary()` in `WorkGroup.tsx` grows a subagent count so that
row reads `2 subagents finished` rather than undercounting.

### The row

New `SubAgentCard.tsx`, sibling to `ToolCallCard`: collapsed row
`Subagent "audit deps" completed · 47s`, expanding to the result text. A `failed`
status gets the error treatment and stays visually distinct, so a failed
delegation is never quietly indistinguishable from a good one.

`spawn_agent`'s own tool card is unchanged.

## Compatibility

**Journals become forward-only at this commit.** New journals contain
`{"type":"SubAgentResult",…}` parts; an older binary fails to deserialize that
agent state — the same failure class as the `#101` outage, where renamed
persisted event variants killed the supervisor on the homelab and sessions had to
be wiped.

This was raised during brainstorming with two mitigations offered (a
`ContentPart::Unknown` `#[serde(other)]` tolerance arm, or a two-release split
shipping the shim first). **Both were declined in favour of shipping directly.**
Recorded here as an accepted risk, not an oversight: rolling a deploy back past
this commit will brick sessions that used subagents, and recovery is wiping them.

Old journal rows are untouched and need no migration — their merged notification
text renders exactly as it does today, as a user bubble.

## Error handling

| Case | Behavior |
|---|---|
| `status` is neither `completed` nor `failed` | Rendered neutral, like completed — an unknown status must not borrow success or failure styling it has not earned |
| Timestamps absent (rows journaled before this ships) | No duration on the row; the same condition `WorkGroup` already applies to tool groups |
| Result over 50 KB | Unchanged — `truncate_result` caps it and embeds the marker, which now rides in `SubAgentResultPart.text` |
| Result arrives with no preceding assistant entry | `groupTurns` opens one, so the row is never dropped |
| Old journal rows with merged notification text | Render as they do today; no rewriting of history |

## Testing

- **Rust unit** — `to_message` omits an empty text part and appends results in
  order; both serializers flatten a `SubAgentResult` to a string asserted *equal
  to `notification_text()`*, which is what pins the wire as byte-identical.
- **Rust fold** — timestamps stamped on spawn/complete/fail; a `SubAgentRecord`
  serialized before this change still loads.
- **Orchestrator unit** — `main_turn` puts owed results in `subagent_results` and
  leaves `message` to the inbox alone; an owed-only turn yields `message: None`;
  `wake_owed_parents` resumes a parent with results and no message.
- **Rust actor** — the existing subagent actor tests assert the parent's input
  message: owed-only turns carry result parts only, mixed turns carry
  text-part-then-result-parts in that order.
- **Web unit** — `transcriptSegments.test.ts` gains subagent-item cases; new
  `groupTurns` cases for owed-only (no bubble), mixed (results above bubble), and
  no-preceding-assistant-entry.
- **Web e2e** — new `s-subagent-results.spec.ts`. No new harness plumbing:
  `spawn_agent` is server-owned, so the mock LLM scripts a `spawn_agent` call and
  the subagent runs on the same mock provider. Covers the row appearing, expanding
  to the result, and no user bubble on an owed-only turn.
