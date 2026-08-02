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

  // The composer stays live — a message sent from it answers the ask, exactly
  // as the card does — but points at the question so the two surfaces are not
  // mistaken for a choice between sending and answering.
  await expect(page.getByTestId("composer-input")).toBeEnabled();
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

test("C6: answering marks the turn running and offers Stop alongside Send", async ({
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

  // Answered, and the turn it resumed is a *running* turn, so Stop appears.
  // Send stays alongside it: a message sent now is queued and answered by the
  // next turn, which is also why the duplicate-tool_result latch this test used
  // to assert is gone — the server decides what a message answers, and only a
  // turn that begins while an ask is pending answers that ask.
  await expectStatus(page, "Running");
  await expect(page.getByTestId("composer-stop")).toBeVisible();
  await expect(page.getByTestId("composer-send")).toBeVisible();

  // The next status report releases it.
  await expect(page.getByTestId("assistant-text")).toContainText("Done with blue.");
  await expectStatus(page, "Idle");
  await expect(page.getByTestId("composer-input")).toBeEnabled();
  await expect(page.getByTestId("composer-stop")).toHaveCount(0);
});
