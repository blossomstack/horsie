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

test("J2: a created session keeps the same row, now read-only", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("hello");
  await createSession(page, appBase);
  await sendMessage(page, "configure me");

  // Same place, same keys — creating the session freezes the values without
  // moving the control that shows them.
  const bar = page.getByTestId("session-config-bar");
  await expect(bar).toBeVisible();
  await expect(bar).toHaveAttribute("data-mode", "locked");

  // Icon-only, so the value is in the accessible name; pressing a key opens
  // the readout rather than a picker.
  await expect(page.getByTestId("config-runtime")).toHaveAttribute(
    "aria-label",
    /Runtime — e2e/,
  );
  await page.getByTestId("config-model").click();
  await expect(page.getByTestId("session-config-bar")).toContainText("mock");
  // Nothing here edits: the draft's option rows do not exist on a locked bar.
  await expect(page.getByTestId("model-option")).toHaveCount(0);
});

test("J3: every config menu opens inside the pane, not under the rail", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("hello");
  await createSession(page, appBase);
  await sendMessage(page, "configure me");

  // The keys sit at both ends of the row and their menus run up to 20rem
  // wide, so one fixed anchor cannot serve both: right-anchoring every icon
  // key sent the left group's menus 20rem leftward, under the opaque session
  // rail, which simply ate their left half. The pane is the real constraint,
  // not the viewport.
  const pane = await page
    .locator("main")
    .evaluate((el) => el.getBoundingClientRect())
    .then((r) => r as DOMRect);

  for (const id of ["config-runtime", "config-model"]) {
    await page.getByTestId(id).click();
    const menu = page.locator(`[data-testid="${id}"] + div`);
    await expect(menu).toBeVisible();
    const box = (await menu.boundingBox())!;
    expect(box.x, `${id} menu clears the rail`).toBeGreaterThanOrEqual(pane.x - 1);
    expect(
      box.x + box.width,
      `${id} menu stays inside the pane`,
    ).toBeLessThanOrEqual(pane.x + pane.width + 1);
    await page.keyboard.press("Escape");
  }
});
