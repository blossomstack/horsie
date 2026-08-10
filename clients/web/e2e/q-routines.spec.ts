// Group Q — routines: the /routines page creates a routine over an agent
// preset seeded here, runs it from its detail page, and shows the run there
// while the sidebar's session list deliberately does not grow.
//
// Counts are compared against a baseline rather than zero: the suite runs
// serially against one long-lived server, so earlier groups have already left
// sessions behind.
import type { Page } from "@playwright/test";
import { expect, test } from "./fixtures";

/** Create the agent preset a routine points at, on the connected vendor. */
async function seedAgent(page: Page, appBase: string, name: string): Promise<void> {
  const cfg = (await (
    await page.request.get(`${appBase}/api/config`)
  ).json()) as { models: { alias: string }[]; vendors: { name: string }[] };
  const model = cfg.models[0]?.alias;
  const vendor = cfg.vendors[0]?.name;
  expect(model, "the e2e harness seeds at least one model").toBeTruthy();
  expect(vendor, "the e2e harness connects at least one vendor").toBeTruthy();
  const res = await page.request.post(`${appBase}/api/agents`, {
    data: { name, model, vendor },
  });
  expect(res.status()).toBe(201);
}

/** How many sessions the sidebar would list right now. */
async function sessionCount(page: Page, appBase: string): Promise<number> {
  const res = await page.request.get(`${appBase}/api/sessions`);
  const body = (await res.json()) as { sessions: unknown[] };
  return body.sessions.length;
}

test("Q1: the sidebar links to the routines page", async ({
  page,
  appBase,
}) => {
  await page.goto(appBase);
  await page.getByTestId("routines-link").click();
  await page.waitForURL((url) => url.pathname === "/routines");
  await expect(page.getByTestId("routines-page")).toBeVisible();
});

test("Q2: a routine is created, run, and its run is listed only under it", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.reset();
  await mock.queueText("done");
  await seedAgent(page, appBase, "e2e-routine-agent");
  const before = await sessionCount(page, appBase);

  // Create through the form.
  await page.goto(`${appBase}/routines`);
  await page.getByTestId("new-routine-button").click();
  await expect(page.getByTestId("routine-edit-page")).toBeVisible();
  await page.getByTestId("routine-name-input").fill("e2e-routine");
  await page.getByTestId("routine-description-input").fill("from e2e");
  await page
    .getByTestId("routine-agent-select")
    .selectOption("e2e-routine-agent");
  await page.getByTestId("routine-prompt-input").fill("say hello");
  // A routine must say where it runs; save stays blocked until it does.
  await expect(page.getByTestId("save-routine-button")).toBeDisabled();
  await page.getByTestId("config-environment").click();
  await page.locator('[data-testid="environment-option"][data-value="e2e"]').click();
  await page.keyboard.press("Escape");
  await page.getByTestId("save-routine-button").click();

  // Lands on the detail page, showing the definition.
  await page.waitForURL((url) => url.pathname === "/routines/e2e-routine");
  const detail = page.getByTestId("routine-detail-page");
  await expect(detail).toContainText("e2e-routine-agent");
  await expect(detail).toContainText("say hello");
  await expect(detail).toContainText("manually");

  // Run it: exactly one run appears here, and the session list does not grow.
  await page.getByTestId("run-routine-button").click();
  await expect(page.getByTestId("routine-run-row")).toHaveCount(1);
  expect(await sessionCount(page, appBase)).toBe(before);

  // The list page shows it, and the run opens as an ordinary session.
  await page.getByTestId("routines-link").click();
  const row = page.locator('[data-testid="routine-row"][data-routine-name="e2e-routine"]');
  await expect(row).toContainText("from e2e");
  await expect(row).toContainText("e2e-routine-agent");
  await row.getByRole("link").click();
  await page.getByTestId("routine-run-row").first().click();
  await page.waitForURL(/\/sessions\/[0-9a-f-]+$/);
  await expect(page.getByTestId("composer-input")).toBeVisible();
});

test("Q3: deleting a routine takes its runs with it", async ({
  page,
  appBase,
}) => {
  await seedAgent(page, appBase, "e2e-doomed-agent");
  const created = await page.request.post(`${appBase}/api/routines`, {
    data: {
      name: "e2e-doomed",
      agent: "e2e-doomed-agent",
      environment: { type: "Runtime", value: { vendor: "e2e" } },
      prompt: "say hello",
    },
  });
  expect(created.status()).toBe(201);
  const run = await page.request.post(
    `${appBase}/api/routines/e2e-doomed/run`,
    { data: {} },
  );
  expect(run.status()).toBe(201);
  const { session } = (await run.json()) as { session: { id: string } };

  await page.goto(`${appBase}/routines`);
  await page.getByTestId("delete-routine-e2e-doomed").click();
  await page.getByTestId("confirm-accept").click();
  await expect(
    page.locator('[data-testid="routine-row"][data-routine-name="e2e-doomed"]'),
  ).toHaveCount(0);

  const gone = await page.request.get(`${appBase}/api/sessions/${session.id}`);
  expect(gone.status()).toBe(404);
});
