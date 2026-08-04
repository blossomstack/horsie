// Group S — session groups: CRUD from the sidebar, membership, and the
// rename/delete annotation fixups end to end against the real server.

import { expect, test } from "./fixtures";
import { createSession, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("S1: group CRUD and session membership", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("hello");
  await createSession(page, appBase);
  const id = await sendMessage(page, "hi");
  const row = page.locator(
    `[data-testid="session-row"][data-session-id="${id}"]`,
  );

  // Create a group from the Sessions header.
  await page.getByTestId("new-group-button").click();
  await page.getByTestId("group-name-input").fill("web");
  await page.getByTestId("group-name-input").press("Enter");
  await expect(page.getByTestId("group-section-web")).toBeVisible();

  // Move the session into it via the row menu.
  await row.hover();
  await page.getByTestId(`session-row-menu-${id}`).click();
  await page.getByTestId("move-to-group-web").click();
  await expect(
    page.getByTestId("group-section-web").locator(`[data-session-id="${id}"]`),
  ).toBeVisible();

  // Rename; the session follows.
  await page.getByTestId("group-menu-button-web").click();
  await page.getByTestId("rename-group-item").click();
  await page.getByTestId("group-rename-input").fill("frontend");
  await page.getByTestId("group-rename-input").press("Enter");
  await expect(
    page
      .getByTestId("group-section-frontend")
      .locator(`[data-session-id="${id}"]`),
  ).toBeVisible();

  // Delete (two-step); the session lands back in Ungrouped.
  await page.getByTestId("group-menu-button-frontend").click();
  await page.getByTestId("delete-group-item").click();
  await page.getByTestId("group-menu-button-frontend").click();
  await page.getByTestId("confirm-delete-group-item").click();
  await expect(
    page
      .getByTestId("group-section-ungrouped")
      .locator(`[data-session-id="${id}"]`),
  ).toBeVisible();
  await expect(row).toBeVisible();
});
