// Group G — the OpenAI-compatible wire (provider kind "openai",
// /v1/chat/completions) works end-to-end through the real server. The mock
// serves both wires off one queue; global-setup seeds a second provider
// `mock-openai` pointed at the same mock, and these tests pick its model.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

const OPENAI_MODEL = "openai-mock";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("G1: a text turn completes over the OpenAI wire", async ({ page, appBase, mock }) => {
  await mock.queueText("Answered via the OpenAI wire.");
  await createSession(page, appBase, { model: OPENAI_MODEL });

  await sendMessage(page, "hello over openai");

  await expect(page.getByTestId("assistant-text")).toContainText(
    "Answered via the OpenAI wire.",
  );
  await expectStatus(page, "Idle");
});

test("G2: a tool call runs on the real runtime over the OpenAI wire", async ({
  page,
  appBase,
  mock,
}) => {
  // Turn = two LLM calls over the OpenAI wire: a tool_call, then the final text.
  await mock.queueToolCall("bash", { command: "echo OPENAI_TOOL_OK" });
  await mock.queueText("Done — the tool ran over openai.");
  await createSession(page, appBase, { model: OPENAI_MODEL });

  await sendMessage(page, "run the tool");

  const card = page.locator('[data-testid="tool-call-card"][data-tool="bash"]');
  await expect(card).toBeVisible();
  await card.getByTestId("tool-call-toggle").click();
  await expect(card.getByTestId("tool-call-output")).toContainText("OPENAI_TOOL_OK");

  await expect(page.getByTestId("assistant-text")).toContainText(
    "Done — the tool ran over openai.",
  );
  await expectStatus(page, "Idle");
});
