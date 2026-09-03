// Group A — turn basics + streaming.
// Real horsie-server + mock LLM + real local runtime vendor, driven via the UI.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("A1: draft creates a session on the local runtime vendor via first message", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("ok");
  await createSession(page, appBase);
  // Draft toolbar is present and editable before anything is created.
  await expect(page.getByTestId("session-config-bar")).toHaveAttribute(
    "data-mode",
    "draft",
  );

  // The empty composer must never paint a vertical scrollbar: the fractional
  // line box overflows the rounded client box by a sub-pixel amount, which
  // browsers with a visible scrollbar (Safari at some zoom levels, any
  // classic-scrollbar setup) render as a permanent scrollbar on an empty
  // input. Below the 200px auto-grow cap overflow is hidden on purpose.
  await expect(page.getByTestId("composer-input")).toHaveCSS("overflow-y", "hidden");

  const id = await sendMessage(page, "first message");

  await expectStatus(page, "Idle");
  await expect(page.getByTestId("composer-input")).toHaveCSS("overflow-y", "hidden");
  await expect(
    page.locator('[data-testid="session-row"]', { hasText: "first message" }),
  ).toBeVisible();
  // The row stays above the composer and flips to read-only.
  await expect(page.getByTestId("session-config-bar")).toHaveAttribute(
    "data-mode",
    "locked",
  );
  await expect(page.getByTestId("config-environment")).toHaveAttribute(
    "aria-label",
    /Environment — e2e/,
  );
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

test("A4: the composer only scrolls once the input passes the auto-grow cap", async ({
  page,
  appBase,
}) => {
  await createSession(page, appBase);

  const input = page.getByTestId("composer-input");
  await expect(input).toHaveCSS("overflow-y", "hidden");

  // Well past the 200px cap: content overflows and the scrollbar returns.
  const manyLines = Array.from({ length: 30 }, (_, i) => `line ${i}`).join("\n");
  await input.fill(manyLines);
  await expect(input).toHaveCSS("overflow-y", "auto");
  await expect
    .poll(async () =>
      input.evaluate((el) => (el as HTMLTextAreaElement).scrollHeight > (el as HTMLTextAreaElement).clientHeight),
    )
    .toBe(true);

  // Clearing the input resets the box to fit content again.
  await input.fill("");
  await expect(input).toHaveCSS("overflow-y", "hidden");
});

test("A5: selected transcript text can be commented on and sent from the header", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("Keep the retry limit at three attempts.");
  await createSession(page, appBase);
  await sendMessage(page, "Review the retry policy");
  await expectStatus(page, "Idle");

  const prompt = page
    .locator('[data-testid="message"][data-role="User"]')
    .getByTestId("collapsible-body");
  await prompt.evaluate((element) => {
    const range = document.createRange();
    range.selectNodeContents(element);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
    element.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
  });
  await page.getByLabel("Add a comment…").fill("Keep this request in scope.");
  await page.getByRole("button", { name: "Add comment" }).click();

  const reply = page.getByTestId("assistant-text");
  await reply.evaluate((element) => {
    const range = document.createRange();
    range.selectNodeContents(element);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
    element.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
  });

  await page.getByLabel("Add a comment…").fill("Increase this to five attempts.");
  await page.getByRole("button", { name: "Add comment" }).click();
  await expect(page.getByTestId("transcript-comment-marker")).toHaveCount(2);
  await page.getByTestId("transcript-comment-marker").last().click();
  await expect(page.getByTestId("transcript-comment-panel")).toContainText(
    "Increase this to five attempts.",
  );
  await page
    .getByTestId("transcript-comment-panel")
    .getByRole("button", { name: "Collapse comment" })
    .click();

  const sendComments = page.getByTestId("send-transcript-comments");
  await expect(sendComments).toHaveAccessibleName("Send 2 comments");
  await mock.queueText("The retry limit is now five attempts.");
  await sendComments.click();

  const sentMessage = page.locator('[data-testid="message"][data-role="User"]').last();
  await expect(sentMessage).toContainText("Keep this request in scope.");
  await expect(sentMessage).toContainText("Increase this to five attempts.");
  await expect(page.getByTestId("send-transcript-comments")).toHaveCount(0);
  await expect(page.getByTestId("transcript-comment-marker")).toHaveCount(0);
  await expect(page.getByTestId("assistant-text").last()).toContainText(
    "The retry limit is now five attempts.",
  );
  await expect
    .poll(() => mock.capturedContains("Keep this request in scope."))
    .toBe(true);
  await expect
    .poll(() => mock.capturedContains("Keep the retry limit at three attempts."))
    .toBe(true);
  await expect
    .poll(() => mock.capturedContains("Increase this to five attempts."))
    .toBe(true);
});
