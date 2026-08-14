// Group S — session tags: created by use, filtered three ways, and gone the
// moment nothing carries them, end to end against the real server.

import { expect, test } from "./fixtures";
import { createSession, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("S1: a tag is created by use, filters both ways, and vanishes when dropped", async ({
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

  // No tag exists yet, so there is nothing to filter by and no button for it.
  await expect(page.getByTestId("tag-filter-button")).toHaveCount(0);

  // Creating a tag *is* assigning one that does not exist. What was typed is
  // normalised to the charset the annotation key accepts.
  await row.hover();
  await page.getByTestId(`session-row-menu-${id}`).click();
  await page.getByTestId("new-tag-input").fill("Web UI");
  await page.getByTestId("new-tag-input").press("Enter");
  await page.keyboard.press("Escape");

  // It exists now, so the filter appears and holds it.
  await page.getByTestId("tag-filter-button").click();
  const chip = page.getByTestId("tag-chip-web-ui");
  await expect(chip).toBeVisible();

  // Require it: the tagged session stays.
  await chip.click();
  await expect(chip).toHaveAttribute("data-state", "require");
  await expect(row).toBeVisible();

  // Exclude it: the tagged session goes.
  await chip.click();
  await expect(chip).toHaveAttribute("data-state", "exclude");
  await expect(row).toBeHidden();

  await page.getByTestId("clear-tag-filter").click();
  await expect(row).toBeVisible();

  // Unassign the only carrier and the tag itself ceases to exist — there is no
  // registry keeping it alive with zero sessions.
  await row.hover();
  await page.getByTestId(`session-row-menu-${id}`).click();
  await page.getByTestId("toggle-tag-web-ui").click();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("tag-filter-button")).toHaveCount(0);
});
