# Hook records translate into the conversation — design

Written 2026-08-05, after #206 merged. Reverses one decision from
`2026-08-04-hook-records-in-history-design.md` and closes #208.

## Why

#140 introduced `HistoryEntry`, a union of `Llm(Message)` and `Hook(HookEntry)`,
on a stated premise: hook records are an audit trail, the model must never see
them, and `AgentState::prompt_messages()` enforces that by dropping every
non-`Llm` entry. Its doc comment still says so — "a hook record leaking into a
prompt is a compile error rather than a review rule".

That premise was true when the only wired events were `PreToolUse` and
`PostToolUse`. #206 wired `Stop` and modelled twelve more, and several of them
exist *specifically* to put text in front of the model. The premise is now
false, and the code shows it in three places.

**`Stop`'s injected context is dropped on the floor.**
`runtime_client::injected_context` (`runtime-client/src/client.rs:252`) already
extracts `additional_context` from `SessionStart`, `UserPromptSubmit` **and**
`Stop`. It has exactly one caller — the `SessionStart` bootstrap at
`server/src/sessions/session_actor.rs:2099`. A `Stop` hook that returns
`additionalContext` is recorded, rendered in the web UI, and never reaches the
model.

**There are three unrelated answers to "how does a hook reach the model".**

| event | mechanism | where |
| --- | --- | --- |
| `PostToolUse` | appended to the tool's stdout | `runtime/src/hooks/tool.rs:121` |
| `SessionStart` | `SharedContext.bootstrap` → system prompt | `session_actor.rs:2093` |
| `Stop` (blocked) | `Resume { message }` → a real user message | `session_actor.rs:2536` |

The third is the tell: for one event the design already concluded that the right
representation is a conversation message, and reached it by hand.

**`SessionStart` fires on every turn.** `SessionContextProvider::provide()` is
per-run — "Per-run context for a session's agent, resolved on the run's own
task" (`session_actor.rs:1988`) — and the `run_hooks(SessionStart)` call inside
it has no guard. So every user turn on a plugin-enabled session re-runs every
`SessionStart` hook, always with `source: "startup"`; `resume`, `clear`,
`compact` and `fork` are never emitted at all. Today this is nearly invisible,
because the context lands in the system prompt and is overwritten rather than
accumulated. It also means the transcript already holds one `SessionStart`
record per turn.

## The rule

> **A hook record translates into the conversation only when its effect has no
> other representation there.**

`prompt_messages()` stops being a filter and becomes that translation: one
exhaustive match over `HookAction`, so wiring a new event cannot put text in
front of the model without someone deciding how.

`HistoryEntry` survives unchanged. What it loses is its justification — it is no
longer a compile-time barrier, just a sum type over two kinds of transcript
entry. Collapsing the two arms was considered and rejected: see *Rejected
alternatives*.

### Never translated — already represented

**Tool-scoped records.** A tool hook edits the *tool's own output*; the tool
result message then carries whatever the tool actually produced. Nothing was
injected into the conversation, so there is nothing to translate.

- `PreToolUse` has no `additionalContext` by spec (`models/fluorite/hooks.fl:56`,
  with a test at `runtime/src/hooks/tool.rs:375`). Its input rewrite is invisible
  by design; its denial becomes `ToolResult::Err`, which *is* the call's result.
- `PostToolUse` and `PostToolUseFailure` context is appended to the tool's output
  by the runtime, and stays there.

This is not a second mechanism sitting awkwardly beside the first. It is the
same rule reaching a different answer, because a tool hook's subject is a tool
call and a turn hook's subject is the conversation.

`injected_context`'s `None` arm (`client.rs:276`) already draws this line; the
translation inherits it.

### Never translated — no model-visible content

`SessionEnd`, `StopFailure`, `Notification` and `CwdChanged` carry
`SideEffectOutcome`, which admits no JSON output at all. `TaskCreated` and
`TaskCompleted` carry `TaskOutcome::Ran`, which carries nothing. Every
`Failed(_)` arm is an outage, not content. And `system_message`, on every event,
is addressed to the user and never to the model — pinned by
`a_system_message_is_recorded_and_never_injected` (`tool.rs:522`).

### Never translated — represented as a turn input

`Stop::Blocked` starts a new turn rather than annotating an existing one, and
horsie already models a turn input as a message: `SessionCommand::ContinueAfterStop`
→ `AgentCommand::Resume { message }`. That is the correct representation and it
stays. `Resume` with nothing to resume on is a no-op by design
(`agent_actor.rs:1243`), so translating the reason instead would mean inventing a
way to start a run from history alone — new machinery to replace something that
already works.

`Stop::CapReached` says the turn is over. There is nothing to say to the model.

### Translated, positionally

| record | becomes | status |
| --- | --- | --- |
| `SessionStart::Ran(ctx)` | a message before the turn's input | wired; firing fixed here |
| `UserPromptSubmit::Ran(ctx)` | a message before the prompt | #208, wired here |
| `Stop::Ran(ctx)` | a message after the concluded turn | wired; **currently dropped** |
| `SubagentStart::Ran(ctx)` | a message in the subagent's history | wired here |
| `SubagentStop::Ran(ctx)` | a message in the subagent's history | modelled, stays unwired |
| `PostToolBatch::Ran(ctx)` | a message after the batch | modelled, stays unwired |

`PostToolBatch` is the one tool-*named* event that translates. It names
`calls: Vec<ToolScope>` — a whole batch of parallel calls — so it has no single
tool result to attach to, and it fires after the last result and before the next
assistant turn, which is a turn boundary.

`UserPromptSubmit::Blocked` is not a translation. A blocked prompt is never
journaled, so the subtraction happens at the seam and never in the fold.

### Message shape

`Role::User`, one `ContentPart::Text`, framed with its plugin and event:

```
<hook-context plugin="impeccable" event="SessionStart">
…the hook's additionalContext…
</hook-context>
```

Framed, because a plugin is third-party and its text must never be
indistinguishable from horsie's own instructions. One message per record rather
than concatenating several — consecutive user messages are already routine on
the wire (parallel tool results produce them, and no provider coalesces:
`providers/anthropic/src/lib.rs:410` maps one `Message` to one API message), and
merging would lose provenance. Ids are derived from the hook entry's id, so a
prompt is debuggable against the transcript.

### Where it lives

A new `workflow/src/hook_translation.rs`, holding one pure function:

```rust
pub fn translate(entry: &HookEntry) -> Option<Message>
```

`agent_actor.rs` is already past 3,600 lines, and the heart of this change is a
15-arm match that wants unit tests with no actor, no journal and no runtime.

## The seam

Positional translation has an ordering problem that the naive version does not
survive. A run's prompt is snapshotted *before* `provide()` runs —
`start_run(wake, ctx, state.prompt_messages())` (`agent_actor.rs:1036`) computes
history at command-handling time and hands it to the task that then calls
`provide()`. `SessionStart` fires inside `provide()`, so its record is journaled
after that snapshot. Translating from history would make the context appear from
turn two onward and never on turn one.

So `SessionStart` has to fire before the snapshot, which means acquiring a
runtime before the run's task exists. That is exactly the blocker #208 records
for `UserPromptSubmit`: "firing it needs the prompt *and* a runtime, and on a
session's first message no runtime has been acquired yet… needs a seam on the
agent's run path." Both events need the same seam, so it is built once.

`AgentCommand::Resume` is the only run-start command in the actor — prompts,
tool results, subagent results and stop continuations all arrive through it
(`session_actor.rs:838/1064/1092/1190/2541/2661`). It splits in two.

- **`Resume`** validates as it does today, then marks the agent *preparing* —
  rejecting a concurrent `Resume` exactly as `running` does — and spawns a
  prepare task. An agent with `use_plugins == false` skips straight to
  `StartRun`: no runtime acquire, no added latency, and no new failure mode for
  the sessions that do not use plugins.
- **The prepare task** acquires the runtime client and fires what is due:
  `SessionStart` if this agent load has not yet (`SubagentStart` instead when the
  agent is a `Sub`), and `UserPromptSubmit` when the input carries a user
  message. It then `tell`s `HooksRan { records }`, then `StartRun { input }`.
- **`StartRun`** does what `Resume`'s tail does today: repair dangling tool
  calls, `prompt_messages()` (now translating), persist the input message,
  `start_run`.

**Ordering is a guarantee, not luck** — the same argument #140 relies on for
"a hook entry always precedes its tool result". Both `tell`s go to the agent's
own mailbox, which is FIFO, so the records are folded before the run snapshots
its history. This requires the prepare task to use a **sink-less** client and
tell `HooksRan` directly: routing through `SessionHookSink` goes agent → session
→ agent, two hops, and would race `StartRun`.

### `SessionStart` fires once per load

A plain `bool` on the actor, deliberately **not** journaled — a rehydrated agent
fires again, which is precisely what `source: "resume"` means.

| how the agent started | `source` |
| --- | --- |
| fresh | `startup` |
| recovered from the journal | `resume` |

`clear`, `compact` and `fork` have no counterpart in horsie and stay unemitted.

`SessionStart` also stops firing for subagents, which it does today because the
call in `provide()` is not gated on `SessionAgentKind`. `SubagentStart` exists in
the model for exactly this and is wired at the same seam — one more arm — rather
than leaving subagents with no bootstrap context at all.

### Two failure paths

`UserPromptSubmit::Blocked` drops the run: no input is journaled and no run
starts, but the record still reaches the transcript, so the user sees why nothing
happened.

A prepare step that cannot get a runtime reports a recoverable run failure —
the same outcome `provide()` produces today for the same cause, one step earlier.

## What implementation changed

Three things the design did not anticipate, recorded here rather than left to
the diff:

- **`AbandonedStart::Failed` must carry `ContextError::terminal`.** Flattening it
  left a session whose sandbox is gone reporting a retryable error, so it never
  reached `Unrecoverable` — caught by an existing e2e test, and now pinned by
  `a_terminal_preparation_failure_stays_terminal`.
- **A turn that fires start hooks resolves the runtime twice.** The seam needs a
  handle before the snapshot and `provide` still resolves its own, so a
  hibernated runtime is resumed on every run. `start_hooks` reuses the cached
  handle when one exists, so only the first turn of a load pays it; `get` never
  provisions. `RuntimeClient::without_hook_sink` exists for that reuse — the
  cached handle carries a sink, and these records must not travel on it.
- **`SubagentStart`'s `agent_type` is the constant `"subagent"`.** horsie has no
  agent-*type* concept until #105's Phase 2, and reporting the model name would
  give a matcher something false to match on.

## What is deleted

- `runtime_client::injected_context` and its tests. Its only caller was the
  `SessionStart` bootstrap.
- `SharedContext.bootstrap`, and the bootstrap section of the system prompt.
- The `run_hooks(ServerHookEvent::SessionStart(…))` call inside `provide()`.

`runtime/src/hooks/tool.rs` is **unchanged**, deliberately. So are the web
client, the CLI and both provider crates: translation happens at prompt assembly
and is never journaled, so `/history` and the SSE stream keep returning
`HistoryEntry` exactly as they do now. The hook row stays the single UI
representation of an injection — `HookNoticeRow.tsx` and `hookSummary.ts`
already render `additionalContext` per event arm — and injected text never
appears as a message the user did not write.

## Testing

Unit tests on `translate()` carry most of the weight: one case per translated
arm, and one asserting that every never-translated arm yields `None`. The
exhaustive match is the design; the test is the design restated.

Behavioural tests, each pinning a specific claim above:

- A fresh session's **first** run sees `SessionStart` context. This is the
  regression the ordering problem would cause, and it pins the whole seam —
  verified load-bearing by reading `state` instead of the locally-folded copy,
  which makes exactly this test fail.
- Two turns produce **one** `SessionStart` record, not two.
- An agent recovered from the journal fires `SessionStart` with `source: "resume"`.
- A `Stop` hook's `additionalContext` reaches the next run's prompt. Write this
  one first: it fails on `main`.
- A `UserPromptSubmit` hook that blocks journals no input, starts no run, and
  still reaches the transcript.
- `use_plugins == false` makes no prepare round-trip at all.
- Tool-scoped records contribute nothing to the prompt — the guard on the
  central division of the rule.
- A subagent fires `SubagentStart`, never `SessionStart`.

`FakeRuntimeVendor` already scripts hook records per event
(`server/src/runtime_vendor/fake.rs:733`), and `mock-llm` can assert on the
prompt actually sent, so none of these need a real plugin on disk.

## Rejected alternatives

**Collapse `HistoryEntry` into a flat `Vec<Message>`, materialising hook context
as real messages when the hook runs.** This is the shape that makes history
literally equal to what the model saw. Rejected because the web UI must keep
distinguishing a hook record from a message — an injection has to render as a
hook row, not as a user bubble — and because a materialised message would have
to be journaled, making every future change to the framing a migration rather
than a redeploy.

**Move the `PostToolUse` append out of the runtime and attach it in the fold, by
`tool_call_id`.** It would put every model-visible decision in one file. Rejected
because it buys a code-location invariant and pays a join plus an ordering
subtlety for it, when the runtime is already the layer that applies input
rewrites and denials to the same call.

**Keep `SessionStart` in the system prompt and fix only its firing.** Smaller,
and defensible while the source is `startup`. It breaks down at `resume`, where
the context is explicitly about *this* moment and would land at position zero;
and it defers a seam #208 needs anyway, at which point this design reopens.

**Translate `Stop::Blocked`'s reason instead of passing it to `Resume`.**
Rejected on the same grounds as the tool-scoped records: it is already correctly
represented. See *Never translated — represented as a turn input*.

## Out of scope

- Wiring `SubagentStop`, `PostToolBatch`, or any of the other unwired events.
  They are modelled and their translation arms are written; each still needs a
  call site, which is #105's Phase 1 remainder.
- `continue: false` / `stopReason`, still parsed and not acted on.
- HTTP hooks (`type: "http"`).
- Anything in #105 Phases 2–4.
