// Group A — turn basics + streaming.
// Real horsie-server + mock LLM + real local runtime vendor, driven via the UI.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("A1: create a session on the local runtime vendor", async ({ page, appBase }) => {
  const id = await createSession(page, appBase, { name: "A1 session" });

  await expectStatus(page, "Idle");
  // The session is listed in the sidebar…
  await expect(
    page.locator('[data-testid="session-row"]', { hasText: "A1 session" }),
  ).toBeVisible();
  // …and ran on the real local runtime vendor (header chip).
  await expect(page.getByText("e2e", { exact: true })).toBeVisible();
  expect(id).toMatch(/[0-9a-f-]{8,}/);
});

test("A2: a text turn renders the mock's reply", async ({ page, appBase, mock }) => {
  await mock.queueText("Hello from the mock LLM — 42.");
  await createSession(page, appBase);

  await sendMessage(page, "hi there");

  await expect(page.locator('[data-testid="message"][data-role="User"]')).toContainText(
    "hi there",
  );
  await expect(page.getByTestId("assistant-text")).toContainText(
    "Hello from the mock LLM — 42.",
  );
  await expectStatus(page, "Idle");

  // An unnamed session titles itself from the first message — client-side
  // optimistic update and, after reload, the server's own persisted title.
  await expect(page.getByTestId("session-title")).toHaveText("hi there");
  await expect(
    page.locator('[data-testid="session-row"]', { hasText: "hi there" }),
  ).toBeVisible();
  await page.reload();
  await expect(page.getByTestId("session-title")).toHaveText("hi there");
});

test("A3: a streamed response accumulates into the final message", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueTextStream(["The ", "quick ", "brown ", "fox."]);
  await createSession(page, appBase);

  await sendMessage(page, "stream please");

  await expect(page.getByTestId("assistant-text")).toContainText("The quick brown fox.");
  await expectStatus(page, "Idle");
});
