// Group E — progress UX: thinking visibility + collapsed work-group rows.
//
// Note: the mock LLM's `thinking` response type has no tool calls, and the
// agent loop only continues a turn when the latest response contains a tool
// call (see agentcore/src/agent.rs) — so a queued `thinking` response always
// ends the turn immediately, whatever else is queued behind it. Thinking can
// therefore only appear as the LAST (and only-non-tool) response of a turn,
// never chained before a `text` or another response the way `tool_call` can.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("E1: a thinking-only turn is hidden by default and revealed via Settings", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueThinking("Let me consider the options.");
  await createSession(page, appBase);

  await sendMessage(page, "think about it");

  // A thinking-only response has no tool calls, so the turn ends right there
  // — no visible text, just the (hidden by default) thinking step.
  await expectStatus(page, "Idle");
  await expect(page.getByTestId("thinking-block")).toHaveCount(0);

  await page.getByTestId("settings-menu-button").click();
  await page.locator('[data-testid="setting-toggle"][data-key="showThinking"]').click();

  const block = page.getByTestId("thinking-block");
  await expect(block).toBeVisible();
  await block.getByTestId("thinking-toggle").click();
  await expect(page.getByTestId("thinking-content")).toContainText(
    "Let me consider the options.",
  );
});

test("E2: several tool-call steps collapse into one work group", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("bash", { command: "echo one" });
  await mock.queueToolCall("bash", { command: "echo two" });
  await mock.queueText("Both steps are done.");
  await createSession(page, appBase);

  await sendMessage(page, "do two steps");

  await expect(page.getByTestId("assistant-text")).toContainText("Both steps are done.");
  // Three LLM iterations (tool, tool, text) collapse the two tool calls into
  // exactly one work group, not two separate rows.
  await expect(page.getByTestId("work-group")).toHaveCount(1);
  await expect(page.getByTestId("work-group-summary")).toHaveText("Ran 2 tools");

  await page.getByTestId("work-group-toggle").click();
  await expect(page.locator('[data-testid="tool-call-card"]')).toHaveCount(2);
  await expect(page.getByTestId("thinking-block")).toHaveCount(0);
});

test("E3: a running tool shows a live status on a multi-item work-group row", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("bash", { command: "echo quick" });
  await mock.queueToolCall("bash", { command: "sleep 5" });
  await createSession(page, appBase);

  await sendMessage(page, "run two things, one slow");

  await expectStatus(page, "Running");
  await expect(page.getByTestId("work-group-summary")).toHaveText("Running bash…");

  await page.getByTestId("composer-stop").click();
  await expectStatus(page, "Stopped");
  // The single evolving row settles into a static summary once no longer live.
  await expect(page.getByTestId("work-group-summary")).toHaveText("Ran 2 tools");
});

test("E4: ask_user always renders as a standalone question, breaking out of a preceding work group", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("bash", { command: "echo before-asking" });
  await mock.queueToolCall("ask_user", {
    question: "Which color do you prefer?",
    choices: ["red", "blue"],
  });
  await createSession(page, appBase);

  await sendMessage(page, "pick a color for me");

  // The question is visible immediately, and the tool call that preceded it
  // is still there too — ask_user breaks the run rather than swallowing it.
  await expect(page.getByTestId("ask-user-card")).toContainText("Which color do you prefer?");
  await expect(page.locator('[data-testid="tool-call-card"][data-tool="bash"]')).toBeVisible();
  await expectStatus(page, "AwaitingInput");
});

test("E5: a turn that ends on a trailing thinking step shows the mixed summary", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("bash", { command: "echo done" });
  await mock.queueThinking("That should be enough.");
  await createSession(page, appBase);

  // Reveal thinking so this run has 2 visible items and actually collapses
  // into a group (a single visible item would render bare — see WorkGroup).
  await page.getByTestId("settings-menu-button").click();
  await page.locator('[data-testid="setting-toggle"][data-key="showThinking"]').click();

  await sendMessage(page, "do one thing and wrap up");

  // The turn ends on the thinking step (no tool calls left to make) — no
  // visible text at all, just the finished work-group summary.
  await expectStatus(page, "Idle");
  await expect(page.getByTestId("assistant-text")).toHaveCount(0);
  await expect(page.getByTestId("work-group-summary")).toHaveText("Thought and ran 1 tool");
});
