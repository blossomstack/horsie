# Hook records in the transcript — design

Written 2026-08-04. Extends the plugin-hooks work (#105, PR #140) so that what a
plugin hook did is visible to the user, in the session transcript, and survives
a reload.

## The requirement

A hook that blocks or rewrites a tool call changes what the agent did. That must
be auditable rather than invisible. Concretely: hook records persist, appear in
`/history`, and stream live on the agent's SSE channel — the same design serving
both, not two mechanisms.

## What exists today

PR #140 runs tool hooks inside the runtime, inline with the call they guard, and
returns `Vec<HookRecord>` alongside the `ToolResult` on the runtime response.
`RuntimeClient::invoke` hands those records to a `HookSink`, which the server
implements as `SessionHookSink` — a `tell` into the *session* actor, journaled as
`SessionDomainEvent::HookRan`.

Nothing reads them. `SessionActor::apply_event` deliberately no-ops the variant,
and `/history` is answered from a different actor entirely.

Three facts shape the design, all verified rather than assumed:

- **`/history` is agent-scoped and state-sourced.** `GET
  /sessions/:id/agents/:agent_id/history` is served from the `AgentActor`'s
  in-memory `AgentState`. There is no journal replay in the read path.
- **The join key did not exist.** `HookRecord` had no tool call id, and the id
  the runtime uses is not the id the transcript uses: `agentcore`'s agent loop
  has the LLM's `tool_call_id` at `agent.rs:628` and *drops* it, because
  `Toolbox::execute` takes only `(name, input)`. `RuntimeClient::invoke` then
  mints its own `Uuid::new_v4()` for cancel tracking. Two id spaces, no overlap.
- **Hook records carry no agent identity.** `SessionCommand::HooksRan(records)`
  is agent-blind, so a subagent's hooks are indistinguishable from the main
  agent's — even though `SessionContextProvider` is already per-agent (`kind`)
  and `scoped_client` already stamps subagents with their own runtime
  `agent_id`.

## The design

### `HistoryEntry`

An agent's transcript stops being `Vec<Message>` and becomes a union, so it can
hold things the model never sees:

```fluorite
// models/fluorite/agent.fl

/// One item in an agent's transcript. Not everything in a transcript is
/// something the model saw: `Hook` entries are recorded for the user and are
/// filtered out on the way to a provider.
#[type_tag = "type"]
union HistoryEntry {
    Llm(Message),
    Hook(HookEntry),
}

/// A plugin hook's intervention, as it appears in the transcript.
struct HookEntry {
    /// Cursor id, same space as `Message.id`. Derived from the record rather
    /// than generated, so replay yields the same id: `hook:{tool_call_id}:{n}`.
    id: String,
    created_at_ms: u64,
    record: HookRecord,
}
```

The union sits *above* `Message`, not inside `ContentPart`. That is the whole
point: `Message`, `ContentPart` and both provider crates are untouched. A variant
inside `ContentPart` would have reached 16 files including
`providers/anthropic/src/lib.rs` and `providers/openai/src/wire.rs`, and would
have required every provider to hold an arm for a value that must never arrive.

`HistoryEntry` is the extension point for future non-model entries — compaction
markers, system notices — which add a flat variant and inherit the guard below
for free.

### Type and field changes

| Where | From | To |
| --- | --- | --- |
| `AgentState` (`workflow/src/agent_actor.rs`) | `messages: Vec<Message>` | `history: Vec<HistoryEntry>` |
| `AgentHistoryPage` | `messages` | `entries: Vec<HistoryEntry>` |
| `HistoryPage` (`models/fluorite/session_api.fl:47`) | `messages: Vec<Message>` | `entries: Vec<HistoryEntry>` |
| `AppendedEvent` (`models/fluorite/session.fl:138`) | `message: Message` | `entry: HistoryEntry` |

New: `AgentDomainEvent::HookRan { record, at_ms }` and
`AgentCommand::HooksRan { records }`. Removed: `SessionDomainEvent::HookRan` —
the session journals nothing about hooks. `SessionCommand::HooksRan` survives but
gains an `agent_id` and becomes pure routing: it persists nothing and forwards to
the named agent.

The cursor is unchanged in meaning. `HistoryEntry` exposes `id()` — `Llm(m) =>
m.id`, `Hook(h) => h.id` — and `history_page()` positions on it. `before` /
`after` / `has_more_before` / `has_more_after` keep their current semantics. The
`hook:` prefix cannot collide with existing message ids (`result:{tool_call_id}`
and provider-assigned ids).

### Data flow

```
runtime: dispatch_with_hooks(registry, state, agent_id, tool_call_id, call)
   └─ returns (ToolResult, Vec<HookRecord>) on ToolCallResponse
        │
runtime-client: RuntimeClient::invoke(tool_call_id, call)
   └─ hook_sink.record(records).await        ← awaited before invoke returns
        │
server: AgentHookSink { session, agent_id }
   └─ session.tell(HooksRan { agent_id, records })
        │
server: SessionActor routes by agent_id → agents.get(key)
   └─ agent.tell(AgentCommand::HooksRan { records })
        │
workflow: AgentActor persists AgentDomainEvent::HookRan { record, at_ms }
   └─ apply_event pushes HistoryEntry::Hook onto state.history
        │
        ├─ /history  → AgentHistoryPage { entries, has_more_* }
        └─ SSE       → AgentFrame::Appended { entry }
```

The sink routes through the session rather than holding the agent's `ActorRef`
directly because `SessionContextProvider` is constructed *before* its
`AgentActor` is spawned (`session_actor.rs:567`, `:658`). It carries the agent id
derived from its own `kind`, and the session performs the routing it already
performs for every other agent-bound command.

### The join key

`Toolbox::execute` gains the call id:

```rust
async fn execute(&self, name: &str, input: Value, tool_call_id: &str)
    -> Result<Value, ToolCallError>;
```

`agent.rs:628` already holds it. `RuntimeClient::invoke` then stops minting a
`Uuid` and uses the LLM's id for cancel tracking, the runtime call, and the
record — one id space with three consumers instead of two that cannot be
correlated.

~13 non-test `Toolbox` impls and ~8 test impls change. All mechanical: most
ignore the new parameter.

### The leak guard

`AgentState::prompt_messages() -> Vec<Message>` becomes the only way to obtain a
`Vec<Message>` from state. `state.history` cannot be cloned into a run because
the types no longer match, so a leak is a compile error rather than a review
rule.

Six call sites convert: `start_run` (`agent_actor.rs:931`, `:985`),
`repair_unanswered_tool_calls` (`:1332`), `repair_unanswered_tool_calls_except`
(`:1136`), and `missing_tool_results` (`:828`, `:1306`).

### Ordering

Hook entries land immediately before the tool result they describe, and this is
guaranteed rather than incidental. `RuntimeClient::invoke` awaits
`hook_sink.record()` before returning, so its `tell` is enqueued on the agent's
mailbox before `PersistSink` `ask`s that same actor to journal `ToolComplete`
(`agent_actor.rs:1558`). The mailbox is FIFO, so the ordering holds under
parallel tool use too: each call's hook entries precede that call's own result.

Because entries also carry `tool_call_id`, the client can render a hook row
against its tool call without depending on position — but the ordering means the
default rendering is already correct.

### Subagents

Supported, and for free. Each agent has its own `AgentActor` and journal, so a
subagent's hooks appear at `/agents/:sub_id/history` with no extra routing. This
is a strict improvement on the session-journal approach, where every agent's
hooks were flattened into one log with no agent identity at all.

## Locked decisions

- **Hook records live in the agent journal, not the session journal.** The tool
  call and its result are agent state; its hooks are part of the same story. The
  session-level placement was an artifact of where `RuntimeClient` happens to be
  constructed, not a decision.
- **The union sits above `Message`, never inside `ContentPart`.** Keeps both
  provider crates and the model path out of the blast radius.
- **`state.history` is renamed, not retyped in place.** Serde ignores the now
  unknown `messages` key and defaults `history` to empty, so an old snapshot
  yields an empty transcript instead of failing `recover()` and taking the
  supervisor down the way renamed event variants did on 2026-08-02. Same data
  loss, no outage.
- **No backward compatibility.** Existing sessions lose their transcripts. This
  was explicitly accepted rather than overlooked.
- **`HookEntry.id` is derived, not generated.** `hook:{tool_call_id}:{n}`, where
  `n` is the record's index within the batch reported for that call, so journal
  replay produces the same cursor ids it produced the first time. A generated
  uuid would give a recovered transcript different cursors than the live one.
- **The record reaches the agent asynchronously.** Nothing waits on an audit
  trail; a hook's record must never be able to slow the tool call it describes.
  The join key and the FIFO ordering are what make that safe.

## Delivery

Three PRs. The first two are behaviour-preserving and independently valuable, so
the risky part lands last and small.

1. **Carry the tool call id.** `Toolbox::execute` takes it; `RuntimeClient`
   stops minting a `Uuid`. No behaviour change.
2. **`HistoryEntry`.** The union, `AgentState.history`, `prompt_messages()`, and
   the wire/TS renames. Still no hook records anywhere — this only makes the
   transcript able to hold non-model entries. No behaviour change. This is where
   old sessions lose their transcripts.
3. **Hook records as history.** `AgentDomainEvent::HookRan`, the agent-aware
   sink, the `agent_frame()` mapping, removal of the session-side journaling,
   and the web transcript row.

PR #140 merges as-is; PR 3 removes the session-side journaling it introduced.

## Testing

- **runtime** — the existing hook tests, plus: a record carries the call id it
  was dispatched with.
- **workflow** — a `Hook` entry in `history` never appears in
  `prompt_messages()`; a hook entry is journaled before its `ToolComplete`;
  `history_page()` pages correctly across mixed entries in both directions; a
  mixed history survives a snapshot round-trip.
- **server** — a subagent's hooks land on the subagent's journal, not the main
  agent's; `AgentDomainEvent::HookRan` maps to `AgentFrame::Appended`.
- **web** — Vitest for the entry renderer; one Playwright case asserting a
  blocking plugin's hook row is present **after a reload**, which is the
  requirement an ephemeral frame alone cannot meet.
- CI's TypeScript drift check covers the regenerated types.

## Out of scope

- Turn and session events (`Stop`, `UserPromptSubmit`, `SessionEnd`). They stay
  server-initiated; the runtime cannot know a turn ended. Tracked by #141.
- Retention or truncation of hook entries. `state.history` is already unbounded
  in `Message`s; hook entries do not change that shape, and capping one without
  the other would be arbitrary.
- Rendering design beyond a transcript row. The console/skin work owns that.
