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
  await expect(page.getByTestId("config-environment")).toBeVisible();
  await expect(page.getByTestId("config-model")).toBeVisible();
  // Repos live inside the environment popover now, so there is no key of their
  // own; skills and MCP are not workspace channels and stay offered.
  await expect(page.getByTestId("config-repos")).toHaveCount(0);
  await expect(page.getByTestId("config-runtime")).toHaveCount(0);
  await expect(page.getByTestId("config-skills")).toBeVisible();
  await expect(page.getByTestId("config-mcp")).toBeVisible();
  await expect(page.getByTestId("config-tools")).toBeVisible();

  await page.getByTestId("config-environment").click();
  await expect(
    page.locator('[data-testid="environment-option"][data-selected="true"]'),
  ).toContainText("e2e");
  // The e2e vendor does not provision, so there is nowhere to check a repo
  // out and the checklist stays away.
  await expect(page.getByTestId("environment-repos")).toHaveCount(0);
  await page.keyboard.press("Escape");

  await page.getByTestId("config-model").click();
  await expect(page.locator('[data-testid="model-option"][data-selected="true"]')).toContainText(
    "mock",
  );
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
  await expect(page.getByTestId("config-environment")).toHaveAttribute(
    "aria-label",
    /Environment — e2e/,
  );
  await page.getByTestId("config-model").click();
  await expect(page.getByTestId("session-config-bar")).toContainText("mock");
  // A session that never narrowed reads "Default" rather than showing nothing:
  // "the tools were not the reason" is an answer worth being able to check.
  await expect(page.getByTestId("config-tools")).toHaveAttribute(
    "aria-label",
    /Tools — Default/,
  );

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

  for (const id of ["config-environment", "config-model"]) {
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

// The tool selection is a tri-state, and the middle value is the one that is
// easy to get wrong: an untouched draft defers to the server rather than
// freezing a list, and that is what keeps the control plane out of a session
// nobody granted it to.
test("J8: the Tools picker opens on the default set, grouped and badged", async ({
  page,
  appBase,
}) => {
  await page.goto(appBase);
  await page.getByTestId("new-session-button").click();
  await page.waitForURL((url) => url.pathname === "/");

  const tools = page.getByTestId("config-tools");
  await expect(tools).toHaveAttribute("aria-label", /Tools — Default/);
  await tools.click();

  await expect(page.getByTestId("tool-group-runtime")).toBeVisible();
  await expect(page.getByTestId("tool-group-control")).toBeVisible();

  const option = (name: string) =>
    page.locator(`[data-testid="tool-option"][data-value="${name}"]`);
  await expect(option("bash")).toHaveAttribute("data-selected", "true");
  // Selecting a `horsie_*` tool is the grant, so it can never start ticked.
  await expect(option("horsie_agents")).toHaveAttribute("data-selected", "false");

  // Read-only is the one-click safe selection: every write tool goes.
  await page.getByTestId("tool-quick-read").click();
  await expect(option("read_file")).toHaveAttribute("data-selected", "true");
  await expect(option("bash")).toHaveAttribute("data-selected", "false");
  await expect(tools).toHaveAttribute("aria-label", /Tools — \d+ selected/);

  // And back to deferring, which no set of ticks can express.
  await page.getByTestId("tool-quick-default").click();
  await expect(tools).toHaveAttribute("aria-label", /Tools — Default/);
});
