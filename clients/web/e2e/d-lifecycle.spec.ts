// Group D — lifecycle + resilience: stop/reattach, delete, multi-session
// switching, LLM-error surfacing, and journal replay across a reload.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("D1: stop a running turn, then run again with a new message", async ({
  page,
  appBase,
  mock,
}) => {
  // A slow tool call keeps the turn Running long enough to stop it.
  await mock.queueToolCall("bash", { command: "sleep 5" });
  await createSession(page, appBase);
  await sendMessage(page, "start a long task");

  await expectStatus(page, "Running");
  await page.getByTestId("composer-stop").click();
  // Stop cancels the turn and nothing else — the session is idle, not parked.
  await expectStatus(page, "Idle");

  // A new message runs against the same runtime and completes.
  await mock.queueText("Ran again and finished.");
  await sendMessage(page, "continue");
  await expect(page.getByTestId("assistant-text")).toContainText("Ran again and finished.");
  await expectStatus(page, "Idle");
});

test("D2: delete a session removes it and navigates away", async ({ page, appBase, mock }) => {
  await mock.queueText("ok");
  await createSession(page, appBase);
  const id = await sendMessage(page, "to delete");

  await page.getByTestId("session-delete").click();
  await page.getByTestId("confirm-accept").click();

  await page.waitForURL((url) => url.pathname === "/");
  await expect(
    page.locator(`[data-testid="session-row"][data-session-id="${id}"]`),
  ).toHaveCount(0);
});

test("D3: two sessions keep separate transcripts and switch in the sidebar", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("Reply in session ONE.");
  await createSession(page, appBase);
  const id1 = await sendMessage(page, "hello one");
  await expect(page.getByTestId("assistant-text")).toContainText("Reply in session ONE.");

  await mock.queueText("Reply in session TWO.");
  await createSession(page, appBase);
  await sendMessage(page, "hello two");
  await expect(page.getByTestId("assistant-text")).toContainText("Reply in session TWO.");

  // Switch back to session one via the sidebar; its transcript is intact and
  // does not bleed the other session's content.
  await page.locator(`[data-testid="session-row"][data-session-id="${id1}"]`).click();
  await page.waitForURL(new RegExp(id1));
  await expect(page.getByTestId("assistant-text")).toContainText("Reply in session ONE.");
  await expect(page.getByTestId("assistant-text")).not.toContainText("Reply in session TWO.");
});

test("D4: an LLM error surfaces instead of hanging", async ({ page, appBase, mock }) => {
  // Status 400 → a non-retryable stream error, so the turn fails fast.
  await mock.queueError(400, "E2E_UPSTREAM_BOOM");
  await createSession(page, appBase);
  await sendMessage(page, "trigger an error");

  await expect(page.getByTestId("session-error")).toBeVisible();
  // The turn ended — the session is not stuck Running.
  await expect(page.getByTestId("status-badge")).not.toHaveAttribute("data-status", "Running");

  // The next turn clears the banner: it belongs to the turn that failed, not
  // to the session, and lingering through later turns looks like the error is
  // still happening.
  await mock.queueText("Recovered after the error.");
  await sendMessage(page, "continue");
  await expect(page.getByTestId("session-error")).toHaveCount(0);
  await expect(page.getByTestId("assistant-text")).toContainText("Recovered after the error.");
  await expect(page.getByTestId("session-error")).toHaveCount(0);
});

test("D6: a message sent during a turn is marked unsent, then answered by the next one", async ({
  page,
  appBase,
  mock,
}) => {
  // Turn 1: a slow tool keeps it Running while the second message goes out.
  await mock.queueToolCall("bash", { command: "sleep 3" });
  await mock.queueText("First turn finished.");
  await mock.queueText("Answered the queued one.");
  await createSession(page, appBase);
  await sendMessage(page, "start something slow");
  await expectStatus(page, "Running");

  await sendMessage(page, "and also look at this");

  // Accepted, not refused — and shown as owed rather than as part of the
  // transcript, so the turn it eventually starts does not look self-inflicted.
  await expect(page.getByTestId("queued-marker")).toBeVisible();
  await expect(
    page.locator('[data-testid="message"][data-queued="true"]'),
  ).toContainText("and also look at this");
  // The composer stays live while a turn runs: queueing is the point.
  await expect(page.getByTestId("composer-input")).toBeEnabled();
  await expect(page.getByTestId("composer-send")).toBeVisible();

  // The next turn carries it out of the queue and the marker goes with it.
  await expect(page.getByTestId("assistant-text").last()).toContainText(
    "Answered the queued one.",
  );
  await expect(page.getByTestId("queued-marker")).toHaveCount(0);
  await expectStatus(page, "Idle");
  expect(await mock.capturedContains("and also look at this")).toBe(true);
});

test("D5: transcript is restored from the journal after a reload", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("Persisted assistant reply.");
  await createSession(page, appBase);
  await sendMessage(page, "remember this");
  await expect(page.getByTestId("assistant-text")).toContainText("Persisted assistant reply.");

  await page.reload();

  await expect(page.getByTestId("assistant-text")).toContainText("Persisted assistant reply.");
  await expect(page.locator('[data-testid="message"][data-role="User"]')).toContainText(
    "remember this",
  );
});
