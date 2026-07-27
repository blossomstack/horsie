# Interactive `ask_user` — clickable choices, typed answers, multi-select

Status: approved design, 2026-07-27

## Problem

When the agent calls `ask_user`, the web UI renders the question and its suggested
answers as inert chips. There is no way to click one. The only way to answer is to
type into the composer, and doing that a second time before the resumed turn
finishes bricks the session with a provider 400.

Three gaps, from the outside in:

1. **Choices are not clickable.** `AskUserCard` renders each choice as a plain
   `<span class="chip">` (`clients/web/src/components/ToolCallCard.tsx:30-38`).
   No click handler has ever existed.
2. **Typed answers are second-class.** They work, but the composer gives no
   indication it is the intended answer surface beyond a placeholder, and the
   answer vanishes from the transcript on reload (see below).
3. **The tool cannot express a multi-select question.** The schema is
   `{question, choices?}` (`server/src/sessions/ask_tool.rs`) with no way to say
   "pick any number of these".

Two adjacent defects fall out of the same code path and are fixed here because
the feature cannot be correct without them:

- **Answers disappear on reload.** An answer is delivered as `InjectToolResult`
  and persisted as a `Role.Tool` message. The web reducer never renders Tool-role
  messages (`useSessionStream.ts:151-167`), so what you see after answering is the
  *optimistic* user echo — and that echo is only ever reconciled by an incoming
  `User` message, which never comes. The bubble is a phantom: refresh the page and
  the answer is gone from the transcript.
- **Answer-then-follow-up 400s the session.** Answering leaves the session in
  `AwaitingInput` with `pending_ask` still set (`session_actor.rs:636-659` returns
  `CommandEffect::none()`), and `AwaitingInput.canSend === true`, so a second
  message re-enters the same branch and injects a *second* `tool_result` for the
  same `tool_call_id` — two concurrent runs on one journal, duplicate tool results
  on the wire, provider 400 on every later turn. This is
  [horsie#61](https://github.com/blossomstack/horsie/issues/61) item 3.

## Scope

In scope: the `ask_user` input schema, the transcript ask card, the composer's
behaviour while an ask is pending, and a client-side latch that closes the
double-send window through the UI.

Out of scope: the server-side fix for #61 item 3. The latch prevents the double
send from this UI; a second message arriving from the API or a second browser tab
can still brick the session. That remains tracked in #61 and is a deliberate
decision, not an oversight.

Also out of scope: emitting `SessionEvent::Asked` on the wire (#61 item 8). This
design removes the only consumer of `pendingQuestion` — the composer banner — so
that item becomes a server-side loose end rather than a user-visible bug.

## Design

### 1. Tool schema

One new optional field in `ask_user_spec()`:

```jsonc
{
  "question":  "string",              // required
  "choices":   ["string", ...],       // optional
  "multiple":  true | false           // optional, default false
}
```

Meaning, stated in the tool description so the model has no room to guess:

| `choices` | `multiple` | Question kind |
|---|---|---|
| absent | — | Free text: the user answers in their own words. |
| present | absent / `false` | Single-select: pick one. |
| present | `true` | Multi-select: pick any number. |

The description must also say that **the user may always answer in free text**,
that `choices` are suggestions rather than a constraint, and that the answer may
therefore not appear in the list.

Nothing else changes server-side. `ask_user` stays terminal and unexecuted,
`Conclusion::Ask` still carries only `question`
(`workflow/src/agent_actor.rs:630-644`), and answers still ride the ordinary
message POST → `InjectToolResult`. The client reads `choices` and `multiple`
directly off the assistant message's tool-call input, which it already holds in
`RenderedToolCall.input`.

### 2. The ask card owns the answer

`AskUserCard` becomes the answer surface. An ask is **pending** when its tool call
has no result yet (`call.output === undefined`) *and* the session status is
`AwaitingInput` — both already available client-side. Because `ask_user` is
terminal, at most one ask is pending at a time.

Pending, by question kind. Every kind has a text input and an explicit **Send**
button — selecting a choice never submits on its own, so a choice can always be
sent together with a note, and a misclick is always recoverable:

- **Free text** — question, text input, Send.
- **Single-select** — choices as radio-style buttons: clicking one selects it (and
  deselects any other), clicking the selected one clears it. Text input, Send.
- **Multi-select** — choices as checkboxes, any number selected. Text input, Send.

Send is disabled only when nothing is selected *and* the text input is empty.

Answered (`call.output !== undefined`): the card renders the question, the choices
as static chips with the selected ones marked, and the answer text — read from the
durable tool result, so **it survives a reload**.

The answer submit path does *not* create an optimistic user bubble. The card's own
in-flight state is the feedback, and the phantom-bubble bug goes away with it.

**Wiring.** `Transcript.tsx:52` already routes `ask_user` calls through a
dedicated `"ask"` segment (`lib/transcriptSegments.ts:45`), so the interactive
card has exactly one render site there. `WorkGroup.tsx:19` renders the same
component for asks that appear inside a collapsed work group — those are always
historical, hence always answered, hence always read-only. Rather than thread an
`onAnswer` callback through both trees, `SessionView` provides a small
`AskAnswerContext` (`{ pending: boolean; submit(text): Promise<void> }`) that
`AskUserCard` consumes; with no provider the card renders read-only, which is the
correct default for every other call site.

### 3. Composer

While an ask is pending the composer is **disabled**, with the hint *"Answer the
question above"*; clicking the hint scrolls the pending ask card into view. One
input surface means there is never a question about whether the ticked checkboxes
or the composer text is what gets sent.

The composer's `ask-question-banner` is removed. It is fed by `pendingQuestion`,
which per #61 item 8 is never emitted live, so it renders empty or shows a
previous turn's question. `SessionDetail.pending_question` stays on the server as
durable state; the client simply stops rendering it.

### 4. What the model receives

The tool result content is plain text:

| Answer | Content |
|---|---|
| single choice | the exact choice label |
| multi-select | selected labels joined with `", "` |
| free text | the text verbatim |
| selection + text | labels, blank line, then the text |

### 5. Double-send latch

On submit, `SessionView` sets an `answering` flag. While set: the card's controls
are disabled and show a spinner, and the composer stays disabled regardless of
status.

The flag clears on the **next `StatusChanged` frame**, or on a stream error, or if
the send request itself fails. `report()` emits a `StatusChanged` frame on every
status write with no dedupe (`session_actor.rs:253-264`), so this releases
correctly both when the turn concludes (`Idle`) and when the agent immediately
asks again (`AwaitingInput` → `AwaitingInput`).

The composer additionally shows **Stop** while the flag is set. Stop is currently
gated on `status === Running` (`Composer.tsx:91`), so a turn resumed from an ask —
which stays `AwaitingInput` for its whole duration — is uncancellable today.

## Testing

`clients/web/e2e/c-ask-user.spec.ts` is rewritten around the new card:

- **C1** single-select: click a choice, then Send → the exact label reaches the
  model, turn resumes, card shows the answer. Also asserts that clicking a choice
  submits nothing on its own, that selection is exclusive, and that the composer
  is disabled while the ask is pending.
- **C2** durability: after answering, reload the page and assert the answer still
  renders in the card (fails today).
- **C3** open question (no choices): the card offers a text input only.
- **C4** multi-select: tick two boxes, Send → `"a, b"` reaches the model.
- **C5** combined: a picked choice plus a typed note are sent together.
- **C6** latch: after submitting, the composer stays disabled and Stop is offered
  until the turn reports back.

Assertions about what reached the model read the `tool_result` blocks out of the
mock's captured requests rather than substring-matching the whole body: a choice
label also appears in the echoed `ask_user` call's `choices`, so a substring
match would pass whichever choice was picked.

Rust: a unit test in `ask_tool.rs` asserting the spec exposes `multiple` and
documents the free-text fallback.

## Risks

- **#61 item 3 remains reachable.** Two tabs, or an API client, can still double-
  inject. Accepted; tracked in #61.
- **Choice labels are the answer payload.** A model that emits two identical
  choice strings produces an ambiguous multi-select join. Dedupe choices on
  render; do not attempt indices, which would leak client encoding into the
  model's input.
- **Long choice lists.** No cap is imposed; the card wraps. If this proves ugly in
  practice it is a styling fix, not a design change.
