// Group D — lifecycle + resilience: stop/reattach, delete, multi-session
// switching, LLM-error surfacing, and journal replay across a reload.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("D1: stop a running turn, then reattach with a new message", async ({
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
  await expectStatus(page, "Stopped");

  // A new message reattaches to the still-connected daemon and completes.
  await mock.queueText("Reattached and finished.");
  await sendMessage(page, "continue");
  await expect(page.getByTestId("assistant-text")).toContainText("Reattached and finished.");
  await expectStatus(page, "Idle");
});

test("D2: delete a session removes it and navigates away", async ({ page, appBase }) => {
  const id = await createSession(page, appBase, { name: "to delete" });

  page.on("dialog", (d) => d.accept()); // auto-accept the native confirm()
  await page.getByTestId("session-delete").click();

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
  const id1 = await createSession(page, appBase, { name: "session one" });
  await sendMessage(page, "hello one");
  await expect(page.getByTestId("assistant-text")).toContainText("Reply in session ONE.");

  await mock.queueText("Reply in session TWO.");
  await createSession(page, appBase, { name: "session two" });
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
