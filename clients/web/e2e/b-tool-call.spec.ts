// Group B — agent tool calls execute on the REAL local runtime.
// The mock returns a bash tool_call; the daemon actually runs it in the
// scratch workspace; the tool-call card shows the real output; the mock then
// returns the final text and the turn completes.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("B1: a bash tool call runs on the runtime and its output renders", async ({
  page,
  appBase,
  mock,
}) => {
  // Turn = two LLM calls: first a tool_call, then the final text.
  await mock.queueToolCall("bash", { command: "echo E2E_TOOL_OK" });
  await mock.queueText("Done — the tool ran.");
  await createSession(page, appBase);

  await sendMessage(page, "run the tool");

  const card = page.locator('[data-testid="tool-call-card"][data-tool="bash"]');
  await expect(card).toBeVisible();
  // Expand the card and confirm the REAL runtime produced the output.
  await card.getByTestId("tool-call-toggle").click();
  await expect(card.getByTestId("tool-call-output")).toContainText("E2E_TOOL_OK");

  await expect(page.getByTestId("assistant-text")).toContainText("Done — the tool ran.");
  await expectStatus(page, "Idle");
});

test("B2: a non-zero-exit command still completes the turn", async ({ page, appBase, mock }) => {
  await mock.queueToolCall("bash", { command: "echo RAN_THEN_FAILED; exit 3" });
  await mock.queueText("The command failed but I recovered.");
  await createSession(page, appBase);

  await sendMessage(page, "run the failing tool");

  const card = page.locator('[data-testid="tool-call-card"][data-tool="bash"]');
  await expect(card).toBeVisible();
  await card.getByTestId("tool-call-toggle").click();
  await expect(card.getByTestId("tool-call-output")).toContainText("RAN_THEN_FAILED");

  // The agent gets the tool output back and produces its follow-up text.
  await expect(page.getByTestId("assistant-text")).toContainText(
    "The command failed but I recovered.",
  );
  await expectStatus(page, "Idle");
});
