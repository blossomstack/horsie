// Group V — keyboard-only navigation.
// The rail is a long control, so the tab order has to have a way past it and
// the popovers have to hand the focus back when they close.

import { test, expect } from "./fixtures";
import { createSession, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

/** Start every Tab walk from the top of the document, wherever the page's own
 * load left the caret. */
async function resetFocus(page: import("@playwright/test").Page) {
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
}

test("V1: the skip link is the first tab stop and leads to the composer", async ({
  page,
  appBase,
}) => {
  await createSession(page, appBase);
  await resetFocus(page);

  await page.keyboard.press("Tab");
  const skip = page.getByTestId("skip-to-main");
  await expect(skip).toBeFocused();
  // Hidden until it is the thing you are on — then it is a real, visible plate.
  await expect(skip).toBeVisible();

  await page.keyboard.press("Enter");
  await expect(page.locator("main#main")).toBeFocused();

  // From there the composer is a handful of stops away, and none of them is in
  // the rail — which is the whole point.
  const composer = page.getByTestId("composer-input");
  for (let i = 0; i < 20 && !(await composer.evaluate((el) => el === document.activeElement)); i++) {
    await page.keyboard.press("Tab");
    expect(
      await page.evaluate(() => !!document.activeElement?.closest("aside")),
    ).toBe(false);
  }
  await expect(composer).toBeFocused();
});

test("V2: the rail drawer leaves the tab order when it is closed on a narrow screen", async ({
  page,
  appBase,
}) => {
  await page.setViewportSize({ width: 480, height: 900 });
  await createSession(page, appBase);

  // Parked off-canvas: rendered, but not something the keyboard can walk into.
  await expect(page.getByTestId("agents-link")).toBeHidden();

  await page.getByTestId("rail-toggle").click();
  await expect(page.getByTestId("agents-link")).toBeVisible();
});

test("V3: a config key opens from the keyboard and says what it opened", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("ok");
  await createSession(page, appBase);
  await sendMessage(page, "hello");

  // A created session's keys open a readout rather than a picker; the wiring
  // that names the surface has to be there on that rendition too.
  const trigger = page.getByTestId("config-model");
  await trigger.focus();
  await page.keyboard.press("ArrowDown");

  const panel = page.getByRole("dialog", { name: /^Model — / });
  await expect(panel).toBeVisible();
  await expect(trigger).toHaveAttribute("aria-haspopup", "dialog");
  await expect(trigger).toHaveAttribute(
    "aria-controls",
    await panel.evaluate((el) => el.id),
  );

  await page.keyboard.press("Escape");
  await expect(panel).toBeHidden();
});

test("V4: the model list is one tab stop, walks under the arrow keys, and gives the focus back", async ({
  page,
  appBase,
}) => {
  await createSession(page, appBase);

  const trigger = page.getByTestId("config-model");
  await trigger.focus();
  await page.keyboard.press("ArrowDown");

  const options = page.locator('[data-testid="model-option"]');
  await expect(options.first()).toBeFocused();
  // Exactly one of them is reachable by Tab; the arrows do the rest.
  expect(
    await page.locator('[data-testid="model-option"][tabindex="0"]').count(),
  ).toBe(1);

  const count = await options.count();
  if (count > 1) {
    await page.keyboard.press("ArrowDown");
    await expect(options.nth(1)).toBeFocused();
    await page.keyboard.press("ArrowUp");
    await expect(options.first()).toBeFocused();
  }

  // Escape from inside the list, then again from a choice that closes it: both
  // put the caret back on the key rather than on <body>.
  await page.keyboard.press("Escape");
  await expect(trigger).toBeFocused();

  await page.keyboard.press("ArrowDown");
  await expect(options.first()).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(options.first()).toBeHidden();
  await expect(trigger).toBeFocused();
});
