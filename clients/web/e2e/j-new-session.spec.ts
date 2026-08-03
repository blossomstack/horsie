// Group J — the inline new-session draft flow: config toolbar, gating, and the
// read-only config bar on an existing session.
import { test, expect } from "./fixtures";
import { createSession, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("J1: the New button opens an editable draft at /", async ({ page, appBase }) => {
  await page.goto(appBase);
  await page.getByTestId("new-session-button").click();
  await page.waitForURL((url) => url.pathname === "/");
  await expect(page.getByTestId("session-config-bar")).toHaveAttribute("data-mode", "draft");
  // Local (e2e) vendor does not provision, so no repo/skill/MCP controls show.
  await expect(page.getByTestId("config-runtime")).toBeVisible();
  await expect(page.getByTestId("config-model")).toBeVisible();
  await expect(page.getByTestId("config-repos")).toHaveCount(0);
});

test("J2: an existing session shows a locked, read-only config bar", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("hello");
  await createSession(page, appBase);
  await sendMessage(page, "configure me");

  // Settled config sits behind the header info key rather than on the strip.
  await page.getByTestId("session-info-button").click();
  const bar = page.getByTestId("session-config-bar");
  await expect(bar).toHaveAttribute("data-mode", "locked");
  await expect(page.getByTestId("config-runtime")).toContainText("e2e");
  // Locked model chip is not a menu button — clicking opens nothing.
  await expect(page.getByTestId("config-model")).toContainText("mock");
});
