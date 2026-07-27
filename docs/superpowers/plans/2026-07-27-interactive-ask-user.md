# Interactive `ask_user` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `ask_user` answerable from the transcript card — clickable single- and multi-select choices plus a typed answer — instead of inert chips and a composer that 400s on a follow-up.

**Architecture:** One optional `multiple` field on the tool schema; everything else is client-side. The transcript's ask card becomes the answer surface (choices + text + Send), the composer stands down while an ask is pending, and a client latch keeps a second message from going out before the resumed turn concludes. Answers are read back from the durable tool result, so they survive a reload.

**Tech Stack:** Rust (`horsie-server`), React 19 + TypeScript (`clients/web`), Playwright e2e against a real server + mock LLM.

**Spec:** `docs/superpowers/specs/2026-07-27-interactive-ask-user-design.md`

## Global Constraints

- Work in the `horsie-ask-user` worktree, branch `feat/interactive-ask-user`.
- The client has **no unit-test framework** — Playwright e2e (`clients/web/e2e/`) is the only web test tier. Every web task is TDD'd against a spec in `clients/web/e2e/c-ask-user.spec.ts`.
- e2e runs serially and the mock LLM queue is global and order-based; every test starts with `await mock.reset()` (already in `beforeEach`).
- The server serves the **built** `dist/`, so a web source change is invisible to e2e until `bun run build` runs. Only set `HORSIE_E2E_SKIP_BUILD=1` immediately after building manually.
- `ask_user` is terminal and never executed — at most one ask is pending per session.
- Keep the existing `data-testid` hooks `ask-user-card` and `ask-user-choice`: `e2e/e-progress-ux.spec.ts:102` and `:29` depend on them.
- Answers are plain text. Never send choice indices — client encoding must not leak into the model's input.

---

## File Structure

**Create**
- `clients/web/src/lib/askUser.ts` — the ask domain logic with no React in it: the tool name, answer composition, answered-choice recovery, pending-ask lookup.
- `clients/web/src/components/AskUserCard.tsx` — the interactive card plus the `AskAnswerContext` that feeds it.

**Modify**
- `server/src/sessions/ask_tool.rs` — `multiple` field + description.
- `clients/web/src/components/ToolCallCard.tsx` — delete the inline `AskUserCard`, delegate to the new component.
- `clients/web/src/lib/transcriptSegments.ts` — import `ASK_USER_TOOL` instead of redeclaring it.
- `clients/web/src/pages/SessionView.tsx` — pending-ask lookup, answer handler, latch, context provider, composer props.
- `clients/web/src/pages/NewSessionView.tsx` — composer prop change.
- `clients/web/src/components/Composer.tsx` — drop the banner, add the ask lock and Stop override.
- `clients/web/src/hooks/useSessionStream.ts` — add `statusSeq`; drop the dead `pendingQuestion` plumbing.
- `clients/web/e2e/c-ask-user.spec.ts` — rewritten around the card.

---

### Task 1: `ask_user` schema gains `multiple`

**Files:**
- Modify: `server/src/sessions/ask_tool.rs:19-45` (`ask_user_spec`)
- Test: `server/src/sessions/ask_tool.rs` (the `mod tests` block at the bottom of the same file)

**Interfaces:**
- Consumes: nothing.
- Produces: the wire contract every later task depends on — tool input `{ question: string, choices?: string[], multiple?: boolean }`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `server/src/sessions/ask_tool.rs`:

```rust
    #[tokio::test]
    async fn spec_offers_multi_select_and_advertises_the_free_text_fallback() {
        let tb = AskUserToolbox::new(Arc::new(EmptyToolbox));
        let spec = tb
            .specs()
            .into_iter()
            .find(|s| s.name == ASK_USER_TOOL)
            .expect("ask_user is offered");
        let props = spec
            .input_schema
            .get("properties")
            .expect("schema has properties");

        assert_eq!(
            props.get("multiple").and_then(|m| m.get("type")),
            Some(&json!("boolean")),
            "multi-select must be expressible"
        );
        // `question` stays the only required field: choices and multiple are both
        // optional, so a plain free-text question is still one field.
        assert_eq!(
            spec.input_schema.get("required"),
            Some(&json!(["question"]))
        );

        let choices_doc = props
            .get("choices")
            .and_then(|c| c.get("description"))
            .and_then(Value::as_str)
            .expect("choices is documented");
        assert!(
            choices_doc.contains("own words"),
            "the model must be told choices are suggestions, not a constraint: {choices_doc}"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-server --lib sessions::ask_tool`
Expected: FAIL — `assertion \`left == right\` failed` on the `multiple` assertion (`left: None`).

- [ ] **Step 3: Write the implementation**

Replace the body of `ask_user_spec` in `server/src/sessions/ask_tool.rs`:

```rust
fn ask_user_spec() -> ToolSpec {
    ToolSpec {
        name: ASK_USER_TOOL.to_string(),
        description: "Pause and ask the user a clarifying question before continuing, when \
            their intent is ambiguous or a decision needs their input. Optional -- for an \
            ordinary reply, just answer normally instead of calling this. Omit `choices` for \
            an open question; supply `choices` to suggest answers, and set `multiple` when \
            several may be picked at once."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["question"],
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to put to the user."
                },
                "choices": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional suggested answers. The user can always reply in \
                        their own words instead, so treat these as suggestions and expect an \
                        answer that is not in the list."
                },
                "multiple": {
                    "type": "boolean",
                    "description": "Set true when the user may pick any number of the \
                        choices; omit or set false when exactly one applies. Has no effect \
                        without `choices`."
                }
            }
        }),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-server --lib sessions::ask_tool`
Expected: PASS — 4 tests (the 3 existing plus the new one).

- [ ] **Step 5: Commit**

```bash
git add server/src/sessions/ask_tool.rs
git commit -m "feat(ask_user): allow multi-select questions"
```

---

### Task 2: Single-select answers from the card

The vertical slice: the shared lib, the interactive card, the context, the `SessionView` wiring, and the composer lock. Split any smaller and neither half is testable.

**Files:**
- Create: `clients/web/src/lib/askUser.ts`
- Create: `clients/web/src/components/AskUserCard.tsx`
- Modify: `clients/web/src/components/ToolCallCard.tsx:1-42,68-70`
- Modify: `clients/web/src/lib/transcriptSegments.ts:1-5`
- Modify: `clients/web/src/components/Composer.tsx` (whole component)
- Modify: `clients/web/src/pages/NewSessionView.tsx:59-61`
- Modify: `clients/web/src/pages/SessionView.tsx`
- Test: `clients/web/e2e/c-ask-user.spec.ts`

**Interfaces:**
- Consumes: Task 1's `{question, choices?, multiple?}` tool input; `RenderedToolCall` from `hooks/useSessionStream` (`{id, name, input, output?, isError?, running}`).
- Produces:
  - `ASK_USER_TOOL: string`, `composeAnswer(selected: string[], text: string): string`, `pickedChoices(answer: string, choices: string[]): Set<string>`, `findPendingAsk(messages: RenderedMessage[]): string | null` — all from `lib/askUser`.
  - `AskAnswerProvider`, `useAskAnswer()`, and `interface AskAnswerApi { pendingId: string | null; submitting: boolean; submit(text: string): void }` — from `components/AskUserCard`.
  - `AskUserCard` testids: `ask-user-card` (with `data-pending="true|false"`), `ask-user-choice` (with `data-value` and `data-selected`), `ask-user-text`, `ask-user-send`, `ask-user-answer`.
  - `Composer` props: `{ status, busy, blockedReason?, askLocked?, showStop?, onSend, onStop, onFocusAsk? }` — `pendingQuestion` is gone.

- [ ] **Step 1: Write the failing tests**

Replace the whole of `clients/web/e2e/c-ask-user.spec.ts`:

```ts
// Group C — the ask_user clarify flow.
// The mock returns an ask_user tool call; the session pauses at Awaiting input
// and renders an *interactive* question card. The user answers by picking
// choices, typing, or both; the turn resumes and the answer is durable.

import { test, expect, type MockLlm } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

/**
 * Every `tool_result` content string the mock has received, in request order.
 *
 * Asserting on this rather than `mock.capturedContains` matters here: a choice
 * label also appears in the echoed `ask_user` tool call's `choices`, so a raw
 * substring match is true whichever choice the user picked. The tool result is
 * the only place the *answer* lives. The mock speaks the Anthropic wire, where
 * a tool result is a `{type: "tool_result", content: string}` block inside a
 * user message (`providers/anthropic/src/lib.rs:224`).
 */
async function answersSent(mock: MockLlm): Promise<string[]> {
  const bodies = (await mock.received()) as {
    messages?: { content?: unknown }[];
  }[];
  const out: string[] = [];
  for (const body of bodies) {
    for (const msg of body.messages ?? []) {
      if (!Array.isArray(msg.content)) continue;
      for (const block of msg.content as { type?: string; content?: unknown }[]) {
        if (block.type === "tool_result" && typeof block.content === "string") {
          out.push(block.content);
        }
      }
    }
  }
  return out;
}

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("C1: a single-select ask is answered by picking a choice and sending", async ({
  page,
  appBase,
  mock,
}) => {
  // First LLM call asks; the second (after the user answers) concludes.
  await mock.queueToolCall("ask_user", {
    question: "Which color do you prefer?",
    choices: ["red", "blue"],
  });
  await mock.queueText("Great — blue it is.");
  await createSession(page, appBase);

  await sendMessage(page, "pick a color for me");

  const card = page.getByTestId("ask-user-card");
  await expect(card).toContainText("Which color do you prefer?");
  await expect(card).toHaveAttribute("data-pending", "true");
  await expectStatus(page, "AwaitingInput");

  // The card owns the input while an ask is pending.
  await expect(page.getByTestId("composer-input")).toBeDisabled();
  await expect(page.getByTestId("composer-ask-hint")).toBeVisible();

  const blue = page.locator('[data-testid="ask-user-choice"][data-value="blue"]');
  const red = page.locator('[data-testid="ask-user-choice"][data-value="red"]');

  // Selecting is not sending: no answer leaves the browser on click.
  await blue.click();
  await expect(blue).toHaveAttribute("data-selected", "true");
  expect(await answersSent(mock)).toEqual([]);

  // Single-select is exclusive, and re-clicking clears.
  await red.click();
  await expect(blue).toHaveAttribute("data-selected", "false");
  await expect(red).toHaveAttribute("data-selected", "true");
  await red.click();
  await expect(red).toHaveAttribute("data-selected", "false");
  await expect(page.getByTestId("ask-user-send")).toBeDisabled();

  await blue.click();
  await page.getByTestId("ask-user-send").click();

  await expect(page.getByTestId("assistant-text")).toContainText("Great — blue it is.");
  await expectStatus(page, "Idle");
  // Exactly the picked label — not "red", and not a JSON envelope or an index.
  expect(await answersSent(mock)).toEqual(["blue"]);
});

test("C2: the answer is rendered on the card and survives a reload", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("ask_user", {
    question: "Which color do you prefer?",
    choices: ["red", "blue"],
  });
  await mock.queueText("Great — blue it is.");
  await createSession(page, appBase);

  const id = await sendMessage(page, "pick a color for me");
  await expect(page.getByTestId("ask-user-card")).toBeVisible();
  await page.locator('[data-testid="ask-user-choice"][data-value="blue"]').click();
  await page.getByTestId("ask-user-send").click();
  await expectStatus(page, "Idle");

  await expect(page.getByTestId("ask-user-answer")).toHaveText("blue");

  // The answer is a durable tool result, not an optimistic echo.
  await page.goto(`${appBase}/sessions/${id}`);
  await expect(page.getByTestId("ask-user-answer")).toHaveText("blue");
  await expect(page.getByTestId("ask-user-card")).toHaveAttribute("data-pending", "false");
});

test("C3: an open question with no choices takes a typed answer", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("ask_user", { question: "What should I name it?" });
  await mock.queueText("Naming it Ferdinand.");
  await createSession(page, appBase);

  await sendMessage(page, "name the thing");

  await expect(page.getByTestId("ask-user-card")).toContainText("What should I name it?");
  await expect(page.getByTestId("ask-user-choice")).toHaveCount(0);
  await expect(page.getByTestId("ask-user-send")).toBeDisabled();

  await page.getByTestId("ask-user-text").fill("Ferdinand");
  await page.getByTestId("ask-user-send").click();

  await expect(page.getByTestId("assistant-text")).toContainText("Naming it Ferdinand.");
  await expectStatus(page, "Idle");
  expect(await answersSent(mock)).toEqual(["Ferdinand"]);
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd clients/web && bun run test:e2e c-ask-user.spec.ts`
Expected: FAIL — C1 times out waiting for `[data-pending]` on `ask-user-card` (the attribute doesn't exist); C2/C3 fail on the missing `ask-user-send`.

- [ ] **Step 3: Write the ask domain lib**

Create `clients/web/src/lib/askUser.ts`:

```ts
import type { RenderedMessage } from "../hooks/useSessionStream";

/** The server's dedicated "ask the user" tool for sessions — kept in sync with
 * `ASK_USER_TOOL` in `server/src/sessions/ask_tool.rs`. */
export const ASK_USER_TOOL = "ask_user";

/** The `ask_user` tool call's input, as the model supplies it. */
export interface AskInput {
  question?: string;
  choices?: string[];
  multiple?: boolean;
}

export function askInputOf(input: unknown): AskInput {
  return input && typeof input === "object" ? (input as AskInput) : {};
}

/** The answer text sent to the model: picked labels joined, then any free text.
 * Plain prose on purpose — choice *indices* would leak client encoding into the
 * model's input. */
export function composeAnswer(selected: string[], text: string): string {
  const picks = selected.join(", ");
  const free = text.trim();
  if (picks && free) return `${picks}\n\n${free}`;
  return picks || free;
}

/** Best-effort recovery of which choices an answer picked, for re-rendering an
 * answered card. The selection is the answer's first block (see
 * `composeAnswer`); a label containing ", " can't be recovered, in which case
 * the chip just renders unmarked — the verbatim answer is shown either way. */
export function pickedChoices(answer: string, choices: string[]): Set<string> {
  const head = answer.split("\n\n")[0] ?? "";
  const parts = new Set(head.split(", "));
  return new Set(choices.filter((c) => parts.has(c)));
}

/** The tool call id of the ask awaiting an answer, or null. `ask_user` is
 * terminal, so only the newest ask can be pending: an older one without a
 * result belongs to an abandoned turn and must stay read-only. */
export function findPendingAsk(messages: RenderedMessage[]): string | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const calls = messages[i].toolCalls;
    for (let j = calls.length - 1; j >= 0; j--) {
      if (calls[j].name !== ASK_USER_TOOL) continue;
      return calls[j].output === undefined ? calls[j].id : null;
    }
  }
  return null;
}
```

- [ ] **Step 4: Write the interactive card**

Create `clients/web/src/components/AskUserCard.tsx`:

```tsx
import { ArrowUp, HelpCircle, Loader2 } from "lucide-react";
import { createContext, useContext, useState } from "react";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { askInputOf, composeAnswer, pickedChoices } from "../lib/askUser";
import { cn } from "../lib/cn";

export interface AskAnswerApi {
  /** Tool call id of the ask awaiting an answer, or null when none is pending. */
  pendingId: string | null;
  /** An answer is in flight — the turn it resumes has not reported back yet. */
  submitting: boolean;
  submit: (text: string) => void;
}

const AskAnswerContext = createContext<AskAnswerApi | null>(null);

export const AskAnswerProvider = AskAnswerContext.Provider;

/** Null outside a session view — every other call site renders read-only, which
 * is the right default for a historical transcript. */
export function useAskAnswer(): AskAnswerApi | null {
  return useContext(AskAnswerContext);
}

/** An `ask_user` call: the question, its suggested answers, and — while it is
 * the pending ask — the controls to answer it. */
export function AskUserCard({ call }: { call: RenderedToolCall }) {
  const api = useAskAnswer();
  const input = askInputOf(call.input);
  // Duplicate labels would make a multi-select join ambiguous.
  const choices = [...new Set(input.choices ?? [])];
  const multiple = input.multiple === true;
  const pending = api != null && api.pendingId === call.id;
  const answer = call.output;

  const [selected, setSelected] = useState<string[]>([]);
  const [text, setText] = useState("");

  const toggle = (c: string) =>
    setSelected((prev) => {
      if (prev.includes(c)) return prev.filter((x) => x !== c);
      return multiple ? [...prev, c] : [c];
    });

  const picked = answer !== undefined ? pickedChoices(answer, choices) : null;
  const canSend =
    pending && !api.submitting && (selected.length > 0 || text.trim().length > 0);

  const send = () => {
    if (!canSend) return;
    api.submit(composeAnswer(selected, text));
  };

  return (
    <div
      data-testid="ask-user-card"
      data-pending={pending}
      className="rounded-[var(--radius)] border border-warning/40 bg-warning-soft px-3 py-2 text-sm text-text"
    >
      <div className="flex items-start gap-2">
        <HelpCircle size={16} className="mt-0.5 shrink-0 text-warning" />
        <div className="min-w-0 flex-1">
          <span className="font-medium text-warning">Asked: </span>
          {input.question ?? ""}

          {choices.length > 0 && (
            <div className="mt-1.5 flex flex-wrap gap-1.5">
              {choices.map((c) => (
                <button
                  key={c}
                  type="button"
                  data-testid="ask-user-choice"
                  data-value={c}
                  data-selected={pending ? selected.includes(c) : picked?.has(c) === true}
                  disabled={!pending || api.submitting}
                  onClick={() => toggle(c)}
                  className={cn(
                    "chip text-left",
                    pending && "cursor-pointer hover:border-warning",
                    (pending ? selected.includes(c) : picked?.has(c)) &&
                      "border-warning bg-warning/15 font-medium",
                  )}
                >
                  {c}
                </button>
              ))}
            </div>
          )}

          {pending && (
            <div className="mt-2 flex items-end gap-2">
              <input
                data-testid="ask-user-text"
                value={text}
                onChange={(e) => setText(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    send();
                  }
                }}
                disabled={api.submitting}
                placeholder={
                  choices.length > 0 ? "Or answer in your own words…" : "Your answer…"
                }
                className="min-w-0 flex-1 rounded-[var(--radius)] border bg-transparent px-2 py-1 text-sm outline-none placeholder:text-faint focus:border-accent disabled:opacity-60"
              />
              <button
                type="button"
                data-testid="ask-user-send"
                onClick={send}
                disabled={!canSend}
                aria-label="Send answer"
                className="btn-primary shrink-0 !px-2.5 !py-1"
              >
                {api.submitting ? (
                  <Loader2 size={15} className="animate-spin" />
                ) : (
                  <ArrowUp size={15} />
                )}
              </button>
            </div>
          )}

          {answer !== undefined && (
            <p
              data-testid="ask-user-answer"
              className="mt-1.5 whitespace-pre-wrap text-muted"
            >
              {answer}
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 5: Delegate from `ToolCallCard` and de-duplicate the tool name**

In `clients/web/src/components/ToolCallCard.tsx`: delete the local `ASK_USER_TOOL` const and the whole `AskUserCard` function (lines 13-42), add the import, and keep the dispatch line pointing at the new component.

```tsx
import { ChevronRight, CircleAlert, CircleCheck, Loader2, Wrench } from "lucide-react";
import { useState } from "react";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { ASK_USER_TOOL } from "../lib/askUser";
import { cn } from "../lib/cn";
import { AskUserCard } from "./AskUserCard";
```

(`HelpCircle` is no longer used here — drop it from the lucide import, as shown.)
The dispatch in `ToolCallCard` is unchanged:

```tsx
  if (call.name === ASK_USER_TOOL) return <AskUserCard call={call} />;
```

In `clients/web/src/lib/transcriptSegments.ts`, delete the local const (lines 3-5) and import it instead:

```ts
import { ASK_USER_TOOL } from "./askUser";
```

- [ ] **Step 6: Stand the composer down while an ask is pending**

In `clients/web/src/components/Composer.tsx`, replace the signature and the parts that used `pendingQuestion`:

```tsx
export function Composer({
  status,
  busy,
  blockedReason = null,
  askLocked = false,
  showStop = false,
  onSend,
  onStop,
  onFocusAsk,
}: {
  status: SessionStatusKind;
  busy: boolean;
  blockedReason?: string | null;
  /** A question in the transcript is awaiting an answer (or one is in flight).
   * The ask card owns the input, so the composer stands down — two live input
   * surfaces would make it ambiguous which one a Send submits. */
  askLocked?: boolean;
  /** Show Stop even when the status isn't `Running`: a turn resumed from an ask
   * stays `AwaitingInput` for its whole duration. */
  showStop?: boolean;
  onSend: (text: string) => void;
  onStop: () => void;
  onFocusAsk?: () => void;
}) {
```

Delete the `ask-question-banner` block (the `{awaiting && pendingQuestion && (…)}` JSX) entirely. Then:

```tsx
  const running = status === SessionStatusKind.Running;
  const awaiting = status === SessionStatusKind.AwaitingInput;
  const blocked = blockedReason != null;
  const stoppable = running || showStop;

  const submit = () => {
    const trimmed = text.trim();
    if (!trimmed || !meta.canSend || busy || blocked || askLocked) return;
    onSend(trimmed);
    setText("");
  };
```

The textarea's `disabled` and `placeholder`:

```tsx
          placeholder={
            askLocked
              ? "Answer the question above"
              : meta.canSend
                ? awaiting
                  ? "Answer the agent…"
                  : "Send a message…  (Enter to send, Shift+Enter for newline)"
                : meta.hint
          }
          disabled={askLocked || (!meta.canSend && !running)}
```

The button pair — `running` becomes `stoppable`, and Send respects the lock:

```tsx
        {stoppable ? (
          <button
            className="btn-outline shrink-0"
            onClick={onStop}
            disabled={busy}
            title="Stop the session (preserves the runtime)"
            data-testid="composer-stop"
          >
            <Square size={15} className="fill-current" />
            Stop
          </button>
        ) : (
          <button
            className="btn-primary shrink-0 !px-3"
            onClick={submit}
            disabled={!text.trim() || !meta.canSend || busy || blocked || askLocked}
            title={blockedReason ?? "Send"}
            aria-label="Send message"
            data-testid="composer-send"
          >
            <ArrowUp size={18} />
          </button>
        )}
```

And add the hint next to the existing `blocked` hint, at the bottom of the component:

```tsx
      {askLocked && (
        <button
          type="button"
          onClick={onFocusAsk}
          data-testid="composer-ask-hint"
          className="mt-1.5 px-2 text-xs text-faint hover:text-muted"
        >
          Answer the question above
        </button>
      )}
```

In `clients/web/src/pages/NewSessionView.tsx`, delete the now-invalid `pendingQuestion={null}` line from its `<Composer …>`.

- [ ] **Step 7: Wire `SessionView`**

In `clients/web/src/pages/SessionView.tsx`, add the imports:

```tsx
import { useMemo } from "react";   // merge into the existing react import
import { AskAnswerProvider } from "../components/AskUserCard";
import { findPendingAsk } from "../lib/askUser";
```

After the existing `const status = …` line (replacing the `pendingQuestion` line, which goes away in Task 4 along with the rest of that plumbing — for now leave it):

```tsx
  const pendingAskId = useMemo(
    () =>
      status === SessionStatusKind.AwaitingInput
        ? findPendingAsk(stream.messages)
        : null,
    [status, stream.messages],
  );
```

Add the answer handler next to `handleSend` — deliberately *without* an optimistic user echo: an answer is persisted as a tool result, never as a user message, so an echo would linger unreconciled forever (and vanish on reload).

```tsx
  const handleAnswer = async (text: string) => {
    if (!id) return;
    setSendError(null);
    try {
      await send.mutateAsync({ id, text });
    } catch (e) {
      setSendError(
        e instanceof ApiRequestError ? e.message : "Failed to send your answer.",
      );
    }
  };

  const focusPendingAsk = () => {
    document
      .querySelector('[data-testid="ask-user-card"][data-pending="true"]')
      ?.scrollIntoView({ behavior: "smooth", block: "center" });
  };
```

Wrap the returned tree in the provider (the outermost element of the `return (…)`), and pass the new composer props:

```tsx
    <AskAnswerProvider
      value={{ pendingId: pendingAskId, submitting: false, submit: handleAnswer }}
    >
      {/* …existing tree… */}
    </AskAnswerProvider>
```

```tsx
        <Composer
          status={status}
          busy={send.isPending}
          askLocked={pendingAskId !== null}
          onSend={(text) => handleSend(id, text)}
          onStop={handleStop}
          onFocusAsk={focusPendingAsk}
        />
```

(`submitting` is hard-coded `false` here; Task 4 replaces it with the real latch.)

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cd clients/web && bun run test:e2e c-ask-user.spec.ts`
Expected: PASS — C1, C2, C3.

- [ ] **Step 9: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: no errors. (Catches any missed `pendingQuestion` prop at a `<Composer>` call site.)

- [ ] **Step 10: Commit**

```bash
git add clients/web/src clients/web/e2e/c-ask-user.spec.ts
git commit -m "feat(web): answer ask_user from the transcript card"
```

---

### Task 3: Multi-select and combined answers

**Files:**
- Modify: `clients/web/e2e/c-ask-user.spec.ts` (append two tests)
- No source change is expected — `multiple` is already honoured by `toggle` and `composeAnswer`. These tests prove it, and exist because that behaviour is the feature's whole point.

**Interfaces:**
- Consumes: Task 2's `composeAnswer` and the card testids.
- Produces: nothing new.

- [ ] **Step 1: Write the failing tests**

Append to `clients/web/e2e/c-ask-user.spec.ts`:

```ts
test("C4: a multi-select ask sends every ticked choice", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("ask_user", {
    question: "Which languages should I target?",
    choices: ["rust", "typescript", "python"],
    multiple: true,
  });
  await mock.queueText("Targeting both.");
  await createSession(page, appBase);

  await sendMessage(page, "pick languages");
  await expect(page.getByTestId("ask-user-card")).toBeVisible();

  const rust = page.locator('[data-testid="ask-user-choice"][data-value="rust"]');
  const ts = page.locator('[data-testid="ask-user-choice"][data-value="typescript"]');

  // Multi-select accumulates rather than replacing.
  await rust.click();
  await ts.click();
  await expect(rust).toHaveAttribute("data-selected", "true");
  await expect(ts).toHaveAttribute("data-selected", "true");

  await page.getByTestId("ask-user-send").click();

  await expect(page.getByTestId("assistant-text")).toContainText("Targeting both.");
  expect(await answersSent(mock)).toEqual(["rust, typescript"]);
  await expect(page.getByTestId("ask-user-answer")).toHaveText("rust, typescript");
});

test("C5: a choice and a typed note are sent together", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("ask_user", {
    question: "Which color do you prefer?",
    choices: ["red", "blue"],
  });
  await mock.queueText("Understood.");
  await createSession(page, appBase);

  await sendMessage(page, "pick a color for me");
  await expect(page.getByTestId("ask-user-card")).toBeVisible();

  await page.locator('[data-testid="ask-user-choice"][data-value="blue"]').click();
  await page.getByTestId("ask-user-text").fill("but only for the header");
  await page.getByTestId("ask-user-send").click();

  await expect(page.getByTestId("assistant-text")).toContainText("Understood.");
  // Picks first, blank line, then the note — see `composeAnswer`.
  expect(await answersSent(mock)).toEqual(["blue\n\nbut only for the header"]);
});
```

- [ ] **Step 2: Run the tests**

Run: `cd clients/web && bun run test:e2e c-ask-user.spec.ts`
Expected: PASS — all five. If C4 fails on `data-selected` for `rust` after clicking `typescript`, `toggle` is treating multi-select as exclusive; re-check the `multiple ? [...prev, c] : [c]` branch in `AskUserCard`.

- [ ] **Step 3: Commit**

```bash
git add clients/web/e2e/c-ask-user.spec.ts
git commit -m "test(web): cover multi-select and combined ask answers"
```

---

### Task 4: The double-send latch

**Files:**
- Modify: `clients/web/src/hooks/useSessionStream.ts` (State, INITIAL, `StatusChanged` case, `SessionStream`, the returned object)
- Modify: `clients/web/src/pages/SessionView.tsx`
- Test: `clients/web/e2e/c-ask-user.spec.ts` (append one test)

**Interfaces:**
- Consumes: Task 2's `AskAnswerApi.submitting` and `Composer`'s `showStop`.
- Produces: `SessionStream.statusSeq: number` — a counter incremented on every `StatusChanged` frame, for consumers that need to observe a status *report* rather than a status *change*.

- [ ] **Step 1: Write the failing test**

Append to `clients/web/e2e/c-ask-user.spec.ts`:

```ts
test("C6: answering latches the composer shut and offers Stop until the turn reports back", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("ask_user", {
    question: "Which color do you prefer?",
    choices: ["red", "blue"],
  });
  // The resumed turn runs a slow tool, so the latched window is observable.
  await mock.queueToolCall("bash", { command: "sleep 3" });
  await mock.queueText("Done with blue.");
  await createSession(page, appBase);

  await sendMessage(page, "pick a color for me");
  await expect(page.getByTestId("ask-user-card")).toBeVisible();
  await page.locator('[data-testid="ask-user-choice"][data-value="blue"]').click();
  await page.getByTestId("ask-user-send").click();

  // Answered, but the turn it resumed is still going: the session stays in
  // AwaitingInput the whole time, so without the latch the composer would
  // happily send a second message and inject a duplicate tool_result.
  await expect(page.getByTestId("composer-input")).toBeDisabled();
  await expect(page.getByTestId("composer-stop")).toBeVisible();

  // The next status report releases it.
  await expect(page.getByTestId("assistant-text")).toContainText("Done with blue.");
  await expectStatus(page, "Idle");
  await expect(page.getByTestId("composer-input")).toBeEnabled();
  await expect(page.getByTestId("composer-stop")).toHaveCount(0);
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd clients/web && bun run test:e2e c-ask-user.spec.ts -g C6`
Expected: FAIL — `composer-input` is enabled the moment the ask card flips to answered (the card stops being pending, so `askLocked` goes false).

- [ ] **Step 3: Add `statusSeq` to the stream**

In `clients/web/src/hooks/useSessionStream.ts`:

Add to `interface SessionStream` (next to `liveStatus`):

```ts
  /** Incremented on every `StatusChanged` frame — including one that reports
   * the *same* status. The server reports without deduping, so this is how a
   * consumer observes "the session said something about its state" rather than
   * "the state differs from last render". */
  statusSeq: number;
```

Add the same field to `interface State`, `INITIAL` (`statusSeq: 0`), and the `StatusChanged` reducer case:

```ts
        case "StatusChanged":
          return {
            ...state,
            liveStatus: ev.value.status,
            statusReason: ev.value.reason ?? null,
            statusSeq: state.statusSeq + 1,
            pendingQuestion:
              ev.value.status === SessionStatusKind.AwaitingInput
                ? state.pendingQuestion
                : null,
          };
```

And to the object the `useMemo` returns:

```ts
      liveStatus: state.liveStatus,
      statusSeq: state.statusSeq,
```

- [ ] **Step 4: Hold the latch in `SessionView`**

In `clients/web/src/pages/SessionView.tsx`, add the state next to `sendError`:

```tsx
  const [answering, setAnswering] = useState(false);
  // The `statusSeq` observed when the answer went out; the latch releases on the
  // next frame after it.
  const answerSeq = useRef<number | null>(null);
```

Replace `handleAnswer` with the latching version:

```tsx
  const handleAnswer = async (text: string) => {
    if (!id) return;
    setSendError(null);
    // Answering leaves the session in AwaitingInput for the whole resumed turn
    // (horsie#61 item 3), so status alone can't tell the composer to stand down
    // and a second message would inject a duplicate tool_result — bricking the
    // session with a provider 400. Latch locally until the turn reports back.
    setAnswering(true);
    answerSeq.current = stream.statusSeq;
    try {
      await send.mutateAsync({ id, text });
    } catch (e) {
      answerSeq.current = null;
      setAnswering(false);
      setSendError(
        e instanceof ApiRequestError ? e.message : "Failed to send your answer.",
      );
    }
  };
```

Add the release effect (next to the other effects):

```tsx
  // Release the answer latch on the next status report — the turn concluded, or
  // the agent asked again (AwaitingInput → AwaitingInput, which `report` still
  // emits a frame for).
  useEffect(() => {
    if (answerSeq.current !== null && stream.statusSeq !== answerSeq.current) {
      answerSeq.current = null;
      setAnswering(false);
    }
  }, [stream.statusSeq]);
```

Also release it on a stream error, which ends the turn without a status frame:

```tsx
  useEffect(() => {
    if (stream.streamError) {
      answerSeq.current = null;
      setAnswering(false);
    }
  }, [stream.streamError]);
```

Feed it through:

```tsx
    <AskAnswerProvider
      value={{ pendingId: pendingAskId, submitting: answering, submit: handleAnswer }}
    >
```

```tsx
        <Composer
          status={status}
          busy={send.isPending}
          askLocked={pendingAskId !== null || answering}
          showStop={answering}
          onSend={(text) => handleSend(id, text)}
          onStop={handleStop}
          onFocusAsk={focusPendingAsk}
        />
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cd clients/web && bun run test:e2e c-ask-user.spec.ts`
Expected: PASS — all six.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src clients/web/e2e/c-ask-user.spec.ts
git commit -m "fix(web): latch the composer while an ask answer resumes a turn"
```

---

### Task 5: Remove the dead `pendingQuestion` plumbing, then green the whole suite

The composer banner was the only consumer of `pendingQuestion`, and it never worked: `SessionEvent::Asked` is declared on the wire but never emitted (horsie#61 item 8), so the value was always null on a live session and a mount-time snapshot after a reload. With the banner gone the client plumbing is dead code.

**Files:**
- Modify: `clients/web/src/hooks/useSessionStream.ts` (`State`, `INITIAL`, the `Asked` and `StatusChanged` cases, `SessionStream`, the returned object)
- Modify: `clients/web/src/pages/SessionView.tsx:101`

**Interfaces:**
- Consumes: Task 4's `statusSeq`.
- Produces: `SessionStream` no longer has `pendingQuestion`.

- [ ] **Step 1: Delete the client-side plumbing**

In `clients/web/src/hooks/useSessionStream.ts`, remove:
- `pendingQuestion: string | null;` from both `SessionStream` and `State`
- `pendingQuestion: null,` from `INITIAL`
- the whole `case "Asked":` arm (the `default: return state` arm already covers an event with no handler)
- the `pendingQuestion:` line from the `StatusChanged` case, leaving:

```ts
        case "StatusChanged":
          return {
            ...state,
            liveStatus: ev.value.status,
            statusReason: ev.value.reason ?? null,
            statusSeq: state.statusSeq + 1,
          };
```

- `pendingQuestion: state.pendingQuestion,` from the returned object

In `clients/web/src/pages/SessionView.tsx`, delete the now-unused line:

```tsx
  const pendingQuestion = stream.pendingQuestion ?? detail?.pendingQuestion ?? null;
```

(`SessionDetail.pending_question` stays on the server — it is durable state, and dropping it is a wire change this work does not need.)

- [ ] **Step 2: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: no errors. Any error here names a leftover reference.

- [ ] **Step 3: Run the full e2e suite**

Run: `cd clients/web && bun run test:e2e`
Expected: PASS. Watch `e-progress-ux.spec.ts` E4 in particular — it asserts on `ask-user-card` and `ask-user-choice`, both of which this work kept.

- [ ] **Step 4: Run the Rust checks**

Run from the repo root:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test -p horsie-server --lib sessions::ask_tool
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src
git commit -m "refactor(web): drop the dead pendingQuestion banner plumbing"
```

- [ ] **Step 6: Push and open the PR**

```bash
git push -u origin feat/interactive-ask-user
```

PR body — one long line per paragraph/bullet, no hard wrapping (GitHub renders newlines as literal breaks):

```
gh pr create --title "Interactive ask_user answers" --body "$(cat <<'EOF'
The web UI rendered `ask_user` choices as inert chips — there was no way to click one, and the only way to answer was the composer, which 400s the session if you follow up before the resumed turn concludes.

The transcript ask card is now the answer surface: choices are selectable (radio-style for a single pick, checkboxes when the model sets the new `multiple` flag), there is always a text input, and an explicit Send submits — so a choice can be sent together with a note, and a misclick is recoverable. The composer stands down while an ask is pending, with a hint that scrolls to the question.

Answers are read back from the durable tool result, so they now survive a reload. Previously the answer bubble you saw was an optimistic echo that nothing ever reconciled (an answer is persisted as a Tool-role message, which the transcript does not render), and it vanished on refresh.

`ask_user` gains one optional `multiple` field. Omit `choices` for an open question, supply them to suggest answers, set `multiple` when several may be picked. The tool description now tells the model that choices are suggestions and that the user may always answer in their own words.

Answering leaves the session in `AwaitingInput` for the whole resumed turn (#61 item 3), so a second message injects a duplicate `tool_result` and bricks the session with a provider 400. A client-side latch closes that window through the UI: after answering, the composer stays shut and offers Stop until the turn reports back. The server-side fix stays tracked in #61 — a second browser tab or an API client can still double-inject.

The composer's `ask-question-banner` is removed along with the client's `pendingQuestion` plumbing. It was fed by `SessionEvent::Asked`, which is declared on the wire but never emitted (#61 item 8), so it rendered empty or showed a previous turn's question.

Design: `docs/superpowers/specs/2026-07-27-interactive-ask-user-design.md`. Plan: `docs/superpowers/plans/2026-07-27-interactive-ask-user.md`.

e2e group C is rewritten around the card: single-select, reload durability, open question, multi-select, choice-plus-note, and the latch.
EOF
)"
```

- [ ] **Step 7: Confirm CI is green**

Run: `gh pr checks --watch`
Expected: all checks pass. If the e2e job fails on a timing-sensitive assertion in C6, lengthen the `sleep` in the queued `bash` tool call rather than removing the assertion — the latched window must stay observable.

---

## Self-Review

**Spec coverage**

| Spec section | Task |
|---|---|
| 1. Tool schema (`multiple`, description) | 1 |
| 2. Card owns the answer; pending detection; Send on every kind; answered rendering; reload durability; no optimistic echo; `AskAnswerContext` wiring | 2 |
| 3. Composer disabled + scroll-to-ask hint; banner removed | 2 (lock), 5 (banner + plumbing) |
| 4. Answer serialization (single / multi / text / combined) | 2 (`composeAnswer`), 3 (proves multi + combined) |
| 5. Latch + Stop during a resumed turn | 4 |
| Testing C1–C6 | 2 (C1–C3), 3 (C4–C5), 4 (C6) |
| Risk: duplicate choice labels | 2 (`[...new Set(choices)]`) |

**Type consistency** — `AskAnswerApi { pendingId, submitting, submit }` is defined in Task 2 and consumed unchanged in Task 4. `composeAnswer(selected, text)` and `pickedChoices(answer, choices)` keep their Task 2 signatures throughout. `Composer`'s prop set is declared once in Task 2 and only *used* (never re-declared) in Task 4. `statusSeq` is introduced in Task 4 and only removed-around in Task 5.

**Note on ordering** — Task 2 leaves `submitting: false` hard-coded and Task 5 removes the `pendingQuestion` line Task 2 leaves alone. Both are called out inline at the point they occur, so a task read in isolation is still correct.
