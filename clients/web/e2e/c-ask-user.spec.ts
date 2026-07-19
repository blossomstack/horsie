// Group C — the ask_user clarify flow.
// The mock returns an ask_user tool call; the session pauses at Awaiting input
// and renders a question card; the user answers in the composer; the turn
// resumes and the agent's follow-up (which references the answer) renders.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("C1: ask_user pauses for input, then resumes with the answer", async ({
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

  // The session pauses for input and renders the question card (the canonical
  // rendering of an ask_user call) with its choice chips.
  await expect(page.getByTestId("ask-user-card")).toContainText("Which color do you prefer?");
  await expect(page.getByTestId("ask-user-choice").first()).toBeVisible();
  await expectStatus(page, "AwaitingInput");

  // Answer in the composer; the turn resumes and concludes.
  await sendMessage(page, "blue");

  await expect(page.getByTestId("assistant-text")).toContainText("Great — blue it is.");
  await expectStatus(page, "Idle");
});
