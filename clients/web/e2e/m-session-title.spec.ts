// Group M — the model-owned session title tool. The server first derives a
// fallback title from the user's message, then the model replaces it and the
// dedicated TitleChanged global event updates the header and sidebar live.

import { test, expect } from "./fixtures";
import { createSession, expectStatus, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("M1: set_session_title renames the session live", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("set_session_title", {
    title: "Fix login redirect",
  });
  await mock.queueText("I’ll investigate the redirect behavior.");
  await createSession(page, appBase);

  const id = await sendMessage(page, "the login redirects to the wrong page");

  await expect(page.getByTestId("session-title")).toHaveText(
    "Fix login redirect",
  );
  await expect(
    page.locator(
      `[data-testid="session-row"][data-session-id="${id}"]`,
    ),
  ).toContainText("Fix login redirect");
  await expect(page.getByTestId("assistant-text")).toContainText(
    "I’ll investigate the redirect behavior.",
  );
  await expectStatus(page, "Idle");
});

test("M2: a session can be renamed by hand when the model never titles it", async ({
  page,
  appBase,
  mock,
}) => {
  // The model answers literally and never calls the title tool — the case that
  // left a session wearing its raw first message as a name for good, with the
  // tool as the only writer there was.
  await mock.queueText("The answer is 4.");
  await createSession(page, appBase);
  const id = await sendMessage(page, "what is 2 + 2");
  await expect(page.getByTestId("assistant-text")).toContainText("The answer is 4.");
  await expect(page.getByTestId("session-title")).toHaveText("what is 2 + 2");

  // Renaming lives on the session's own actions menu in the rail, beside its
  // tags and its delete — the header title is a title, not a control.
  await page
    .locator(`[data-testid="session-row"][data-session-id="${id}"]`)
    .hover();
  await page.getByTestId(`session-row-menu-${id}`).click();
  const input = page.getByTestId("session-title-input");
  await input.fill("Arithmetic");
  await input.press("Enter");

  await expect(page.getByTestId("session-title")).toHaveText("Arithmetic");
  await expect(
    page.locator(`[data-testid="session-row"][data-session-id="${id}"]`),
  ).toContainText("Arithmetic");

  // Durable, not just on screen.
  await page.reload();
  await expect(page.getByTestId("session-title")).toHaveText("Arithmetic");
});
