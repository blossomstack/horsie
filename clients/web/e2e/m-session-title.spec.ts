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
