# Agent compaction

Keep a long session inside its context window without losing what it did.

## The problem

An agent's context window is finite; its session is not. Today horsie has no
answer at all — `AgentState::prompt_messages()` hands a provider every LLM
message the log has ever held, so a session ends when the window fills. The
docs say as much: "no automatic mid-run summarisation of the conversation you
can tune". `SessionStartSource::Compact`, `PreCompact` and `PostCompact` all
exist in the wire vocabulary as arms nothing constructs, each carrying a comment
that horsie has no context compaction.

The requirement that shapes everything below: **compaction must not lose
history.** What the model is handed may shrink. What a person can read must not.

## Approach

Nothing is ever deleted from the log. Compaction *appends a boundary* to it, and
`prompt_messages()` learns to start reading from the newest boundary rather than
from zero.

This is available because the log is already the right shape. `AgentState.log`
is append-only, each entry carries a durable `seq` assigned by the fold, and
`prompt_messages()` is the single exhaustive match that turns the log into what
a provider sees. `agent_log.rs` was written anticipating this — `seq` is stored
rather than implied by position specifically so that trimming for context
management stays possible. The documented principle, "the record is complete;
the working set is not", becomes literally true rather than a statement about
journal snapshots.

What the model is handed after a compaction is a **summary plus a recency
window**: a synthetic message carrying the summary and the exact carried state,
followed by the most recent raw messages verbatim. Summary-only was rejected —
the agent reliably stumbles on the first turn after one, because the file path
or error it was mid-way through lives in the last few messages and a summary
paraphrases it away.

### Anchored summarization falls out for free

Because `prompt_messages()` already begins with the previous boundary's summary,
the summarizing call sees `[previous summary, retained raw, new span]` and folds
them into one. That is anchored iterative summarization — merging into a
persistent state rather than re-deriving from the whole history — which the
literature finds beats full reconstruction on continuity, and which also keeps
the cost of a compaction flat no matter how many have already happened. No span
is ever summarized twice.

## Data model

### The boundary

A fourth arm on `AgentLogBody` in `crates/models/fluorite/agent.fl`:

```
union AgentLogBody {
    Llm(Message),
    Hook(HookEntry),
    Lifecycle(LifecycleEvent),
    Compaction(CompactionEntry),
}

struct CompactionEntry {
    /// Prose, from the model. Lossy by nature.
    summary: String,
    /// Exact facts, rendered from state, never routed through the summarizer.
    carried_state: String,
    /// Last entry folded into `summary`.
    covers_through_seq: u64,
    /// First entry still shown to the model raw. Never greater than
    /// `covers_through_seq + 1`; equal to it when nothing was retained.
    retained_from_seq: u64,
    trigger: CompactionTrigger,
    /// From `/compact <instructions>`; absent for an automatic compaction.
    instructions: Option<String>,
    tokens_before: u32,
    tokens_after: u32,
}

#[type_tag = "kind"]
union CompactionTrigger { Auto(EmptyOutcome), Manual(EmptyOutcome) }
```

Adding a union arm is safe for the snapshot contract; renaming one is what took
the supervisor down on 2026-08-02, and nothing here renames anything.

### What `prompt_messages()` becomes

One step ahead of the existing match:

1. Find the last `Compaction` entry. With none, behave exactly as today.
2. Emit its synthetic message (below) as a `user` message.
3. Run the existing filter over entries with `seq >= retained_from_seq`.

A `Compaction` entry that is *not* the newest translates to nothing — a
superseded boundary is history, not context. This is the same discipline the
`Lifecycle` arm already follows, and it is why the union sits above `Message`:
no provider ever holds an arm it must interpret.

### The synthetic message

Two labelled sections in one message:

```
## Summary of earlier work
<summary>

## Current state
<carried_state>
```

Two sections rather than one blob because they have different truth conditions.
The summary is the model's prose and may be wrong at the edges. The carried
state is exact and must be reproduced verbatim — a summarizer that renders
"task 3: in progress" as prose has destroyed the id the agent needs to call
`task_list` correctly.

### Carried state

Rendered by the agent actor, which is the only thing that owns it:

- the task list — `TaskListState::render()` already produces exactly this
- armed timers: id, fire time, message
- pending asks: `tool_call_id` and question
- the working directory and any `set_env` overrides in force
- subagents still running that this agent is waiting on

This exists because the state surviving is not the same as the model knowing it
survived. `task_list`, `timers`, `set_working_dir` and `set_env` are durable
agent state, but the model's *awareness* of them is carried entirely by the tool
calls and results in the history — which is what a compaction summarizes away.
Preserving the state while dropping the knowledge would leave an agent that has
three open tasks and no idea it does.

Snapshotted into the entry at compaction time rather than re-rendered on every
prompt build. Re-rendering would invalidate the provider's cache below the
boundary on every turn, and it is unnecessary: any change *after* the boundary
arrives as a visible tool call in the retained history. The frozen block is the
baseline; the deltas are in plain sight.

### Why no per-message conversation id

The recency window makes a message belong to two consecutive working sets, so a
per-message conversation id cannot be a partition — it would have to either
duplicate the message or re-stamp it. Position already answers every question:

- **A conversation** is the span `(previous boundary.seq, this boundary.seq]`,
  and its id *is* the boundary's `seq`.
- **A message's conversation** is a binary search over the boundary seqs, which
  `agent_log.rs` is already built for.
- **A retained message** needs no second identity. It is in conversation N and
  quoted into N+1, which is what actually happened.

Addressing a message later is `(boundary_seq, entry_seq)`. No new field on
`Message`, no backfill, no snapshot-compat risk.

## Triggering

One check, in `Agent::run_inner`'s loop, at the top, before the request is
built:

```rust
loop {
    if cancel.is_cancelled() { … }
    if iteration >= self.config.max_iterations { … }
    self.maybe_compact(spent.context_tokens, events).await;   // ← here
    let request = CompletionRequest { messages: &self.history, … };
```

Three reasons it belongs there and nowhere else:

- **History is balanced.** The loop appends tool results before coming back
  around, so at the top of every iteration each `tool_use` has its
  `tool_result`. It is the only point in a run where that holds, and it is what
  keeps compaction away from the dangling-`tool_use_id` failures that have bitten
  this codebase before.
- **It subsumes the turn boundary.** `spent.context_tokens` is seeded at run
  start from the actor's durable `state.context_tokens`, so iteration 0 of a
  fresh turn tests the size the previous turn left behind. Turn-boundary
  compaction is not a second mechanism; it is this one at iteration 0.
- **The parts are in hand** — `self.provider`, `self.history`, `events`.

**Accepted imprecision:** the check reacts one iteration late, because
`context_tokens` is the last provider call's input size and does not count tool
results appended since. The threshold's headroom covers it. A token estimator
was considered and rejected: a chars/4 approximation drifts per provider and per
tokenizer, and would put a second number on screen disagreeing with the one the
context gauge shows.

### Thresholds

Server constants, not session settings:

- **Trigger** at 80% of the model card's `context_window`.
- **Retain** approximately the trailing 20% of the window as raw messages.

Both are properties of a model, not of a session, so they stay centrally tunable
rather than frozen into everyone's saved presets.

**A model card with no `context_window` disables automatic compaction for that
session.** No guessed default: guessing wrong either compacts a session that had
room or fails to compact one that did not. `/compact` still works by hand.

### Choosing the cut

Walk back from the tail accumulating tokens until the retain budget is reached,
then move the cut *backwards* to the nearest user-message boundary. Never split
an assistant message from its tool results. When no safe cut exists — a single
turn larger than the retain budget — retain nothing and let the compaction be
summary-only, which is correct: there is no partial view of that turn that is
coherent.

### Failure

If the summarizing call fails, emit nothing, log a warning, and continue the
turn uncompacted. No retry: the retry budget belongs to the turn, and a turn
that then fails with an honest context-overflow error is better than one that
silently proceeds degraded.

## Compacting

1. Fire `PreCompact`. A hook that blocks or halts abandons the compaction.
2. Ask the actor for `carried_state()`. Read at the boundary, not at run start —
   the model can add tasks mid-turn, and mid-loop compaction must see them.
3. Choose the cut.
4. Summarize everything before the cut with a fixed structured prompt (intent ·
   decisions taken · files and code touched · errors and their fixes · work in
   flight · next step), plus the user's `/compact` instructions when present. A
   plain completion on the session's own model, no tools, events routed to a
   null sink so the call never appears in the transcript.
5. Emit `AgentEvent::Compacted { entry }`. The actor folds it into a
   `Compaction` log entry.
6. Rewrite `self.history` in place from the new boundary.
7. Fire `PostCompact`.

### The seam into agentcore

A new optional trait object on `Agent`, the same shape `Toolbox` and
`EventSink` already take:

```rust
#[async_trait]
pub trait CompactionPolicy: Send + Sync {
    /// Exact state that must survive verbatim, rendered by whoever owns it.
    async fn carried_state(&self) -> String;
    async fn before(&self, plan: &CompactionPlan) -> PreCompactDecision;
    async fn after(&self, result: &CompactionResult);
}
```

The server implements it against the runtime client and the agent actor.
`carried_state()` is one mailbox round-trip on a rare path. Hook records reach
the transcript through the existing `HookSink`, so nothing new is built for
them. An agent with no policy — a workflow step, a test fixture — simply never
compacts.

## Settings

`auto_compact: Option<bool>` on `AgentSettings` and on `AgentPresetInput`;
absent means on. A preset seeds the session's value exactly as `thinking_effort`
and `instructions` already do.

It reaches the agent through `Contexts`, which gains `context_window:
Option<u32>`. `SessionContextProvider` already reads the config store to resolve
the model, so this is one more lookup there — and it removes today's oddity that
the HTTP layer is the only thing that knows this number.

## The `/compact` command

### Built-in command registry

One table of `(name, description, handler)`. Merged into the catalogue the `/`
typeahead reads, and consulted **before** the plugin catalogue in
`expand_invocation`, so an installed bundle cannot shadow a builtin.

Built-ins must be offered **even when the session has no plugins selected and
`use_plugins` is false**. Today the catalogue is empty in that case, which would
make `/compact` invisible in exactly the plainest session.

The registry ships with one entry. The next builtin is a row rather than a
fourth special case, and the shadowing rule, the typeahead and the docs are
written once.

### Behaviour

`/compact [instructions]` is not a prompt and never reaches `expand_invocation`.
It becomes a new `Incoming::Compact { id, instructions }` in the agent's inbox.
At the next turn boundary the actor builds an `Agent` exactly as it builds one
for a run and calls `agent.compact(instructions)` instead of `agent.run(…)` —
same builder, same sink, same journal path. A turn in flight finishes first,
which is why this is queued rather than a direct endpoint: no new route, no new
race, and it is journaled in order with everything else.

## Hooks

`PreCompact` and `PostCompact` come off the `NoConcept` list in
`crates/support/src/plugin/hooks/events.rs` and gain input types in
`runtime.fl`. They fire from the `CompactionPolicy` implementation.

`SessionStartSource::Compact` stays unconstructed. `PostCompact` already covers
reacting afterwards, and re-firing `SessionStart` mid-session would re-run every
session-start hook for something that is not a session start. Its comment is
updated to say so rather than to claim horsie has no compaction.

## UI

### Transcript

`transcriptSegments.ts` gains a `compaction` segment, rendered as a full-width
divider: a rule with a centred label — `Compacted · 214 messages · 118k → 12k
tokens` — expandable to show the summary and carried state. A boundary marker,
not a message: no avatar, no bubble.

Everything above and below keeps rendering unchanged, because the history
endpoint already returns the whole log and nothing was removed from it. That is
the entire implementation of "show all history across compactions" — no new
endpoint, no paging change.

### The spine

`TranscriptSpine.tsx` — named a spine, not a rail, because `rail.tsx` is already
the session list. A ~10px column down one edge of the transcript:

- a cap at each end: jump to the very start, jump to the very end
- one tick per compaction boundary, positioned proportionally, scrolling to it
- the current conversation's span drawn slightly brighter, so position is
  legible in a session that has compacted six times
- hover on a tick: `Conversation 2 · 41 messages · 3 Aug`

With zero compactions it is just the two caps. The control does not appear and
disappear as a session's shape changes, which is why it is a spine rather than a
floating prev/next pair.

### Context gauge

`ContextGauge` gains the compaction threshold as a tick on the existing gauge,
so "why did it just compact" is answerable by looking. No number added.

### Forms

One checkbox — "Compact automatically when the context fills" — in the
session-create form and the agent-preset form, disabled with a hint when the
selected model's card carries no context window.

## Testing

- **`prompt_messages()`** — no boundary is today's behaviour; one boundary; two
  boundaries, only the newest honoured; a retained window overlapping the
  previous boundary; a non-newest `Compaction` entry translating to nothing.
- **Cut selection** — never splits a tool call from its results; falls back to
  summary-only when one turn exceeds the retain budget; a compaction on an empty
  log is a no-op.
- **Carried state survives verbatim** — compact a session holding three tasks,
  two armed timers and a non-default working directory against a scripted
  summarizer returning deliberately vague prose, then assert the post-boundary
  prompt still names every task id, timer id and the path exactly. This test
  fails against a design that lets the summary carry that state, which is the
  point of writing it.
- **`Agent::run_inner`** — with a scripted provider and a small fake window:
  compaction fires mid-loop, emits its boundary exactly once, leaves the
  rewritten history balanced, and a failing summarizer leaves the run untouched.
- **Actor fold** — `Compacted` folds to a `Compaction` entry at the right `seq`;
  the fold is deterministic under replay; a snapshot written after a compaction
  recovers to an identical `prompt_messages()`.
- **Journal compatibility** — a snapshot holding no `Compaction` entries
  recovers unchanged.
- **e2e** — a session compacts, then answers a question answerable only from the
  summary; the transcript still serves the pre-compaction messages; `/compact`
  typed in the composer produces a boundary.
- **Web** — the segment reducer emits a compaction segment; the spine renders N
  ticks for N boundaries and none for zero.

## Out of scope

Tool-result clearing, a retrieval layer, per-session threshold tuning, `/clear`
and `/fork`, and constructing `SessionStartSource::Compact`.
