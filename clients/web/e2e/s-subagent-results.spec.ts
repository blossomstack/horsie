// Group S — a finished subagent's result renders as agent work, not a user turn.
//
// The result reaches the parent inside a *user* message (the providers require
// it), so before it became a structured part the transcript showed it as a user
// bubble — a delegating session read as if the person kept pasting reports to
// themselves. These tests pin the rendering, not the wire.
//
// Queue ordering note: the mock LLM has one global FIFO, and two calls race for
// it — the main agent's turn continues the moment `spawn_agent` returns, while
// the subagent starts running at the same instant. Both racers are given the
// *same* text so the race cannot change what is asserted. The third response is
// ordered: the main agent is only woken once the subagent has finished.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

const RACING = "working on it";
const FINAL = "the audit is in";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("S1: a finished subagent renders as a collapsed row, not a user bubble", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("spawn_agent", {
    label: "audit",
    task: "audit the dependencies",
  });
  // Consumed in either order by the main agent's continuation and the
  // subagent's only call — identical, so which gets which does not matter.
  await mock.queueText(RACING);
  await mock.queueText(RACING);
  await mock.queueText(FINAL);

  await createSession(page, appBase);
  await sendMessage(page, "delegate the dependency audit");

  const card = page.locator('[data-testid="subagent-card"][data-subagent="audit"]');
  await expect(card).toBeVisible();
  await expect(card).toHaveAttribute("data-status", "completed");

  // Expanding shows the result the parent was handed.
  await card.getByTestId("subagent-toggle").click();
  await expect(card.getByTestId("subagent-result")).toContainText(RACING);

  await expect(page.getByTestId("assistant-text").last()).toContainText(FINAL);
  await expectStatus(page, "Idle");
});

test("S2: the result turn adds no second user bubble", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("spawn_agent", {
    label: "audit",
    task: "audit the dependencies",
  });
  await mock.queueText(RACING);
  await mock.queueText(RACING);
  await mock.queueText(FINAL);

  await createSession(page, appBase);
  await sendMessage(page, "delegate the dependency audit");

  // Wait for the whole exchange to settle before counting, so this is not
  // asserting on a transcript that simply has not caught up yet.
  await expect(page.locator('[data-testid="subagent-card"]')).toBeVisible();
  await expect(page.getByTestId("assistant-text").last()).toContainText(FINAL);
  await expectStatus(page, "Idle");

  // Exactly one: what the person actually typed. The owed-only turn that
  // delivered the subagent's result carries no typed text, so it gets no bubble.
  const userTurns = page.locator('[data-testid="message"][data-role="User"]');
  await expect(userTurns).toHaveCount(1);
  await expect(userTurns.first()).toContainText("delegate the dependency audit");
});
