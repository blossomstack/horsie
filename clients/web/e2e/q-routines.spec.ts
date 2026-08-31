import { projectRoot } from "./helpers";
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
async function seedAgent(page: Page, apiBase: string, name: string): Promise<void> {
  const cfg = (await (
    await page.request.get(`${apiBase}/config`)
  ).json()) as { models: { alias: string }[]; vendors: { name: string }[] };
  const model = cfg.models[0]?.alias;
  const vendor = cfg.vendors[0]?.name;
  expect(model, "the e2e harness seeds at least one model").toBeTruthy();
  expect(vendor, "the e2e harness connects at least one vendor").toBeTruthy();
  const res = await page.request.post(`${apiBase}/agents`, {
    data: { name, model, vendor },
  });
  expect(res.status()).toBe(201);
}

/** How many sessions the sidebar would list right now. */
async function sessionCount(page: Page, apiBase: string): Promise<number> {
  const res = await page.request.get(`${apiBase}/sessions`);
  const body = (await res.json()) as { sessions: unknown[] };
  return body.sessions.length;
}

test("Q1: the sidebar links to the routines page", async ({
  page,
  appBase,
}) => {
  await page.goto(appBase);
  await page.getByTestId("routines-link").click();
  await page.waitForURL((url) => url.pathname === projectRoot() + "/routines");
  await expect(page.getByTestId("routines-page")).toBeVisible();
});

test("Q2: a routine is created, run, and its run is listed only under it", async ({
  page,
  appBase,
  mock,
  apiBase,
}) => {
  await mock.reset();
  await mock.queueText("done");
  await seedAgent(page, apiBase, "e2e-routine-agent");
  const before = await sessionCount(page, apiBase);

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
  await page.waitForURL((url) => url.pathname === projectRoot() + "/routines/e2e-routine");
  const detail = page.getByTestId("routine-detail-page");
  await expect(detail).toContainText("e2e-routine-agent");
  await expect(detail).toContainText("say hello");
  await expect(detail).toContainText("manually");

  // Run it: exactly one run appears here, and the session list does not grow.
  await page.getByTestId("run-routine-button").click();
  await expect(page.getByTestId("routine-run-row")).toHaveCount(1);
  expect(await sessionCount(page, apiBase)).toBe(before);

  // The list page shows it, and the run opens as an ordinary session.
  await page.getByTestId("routines-link").click();
  const row = page.locator('[data-testid="routine-row"][data-routine-name="e2e-routine"]');
  await expect(row).toContainText("from e2e");
  // The schedule is the one fact the row keeps. What the routine runs is in
  // the panel beside it, which is where the detail below is read.
  await expect(row).toContainText("manually");
  await expect(row).not.toContainText("e2e-routine-agent");
  await row.getByRole("link").click();
  await page.getByTestId("routine-run-row").first().click();
  await page.waitForURL(/\/sessions\/[0-9a-f-]+$/);
  await expect(page.getByTestId("composer-input")).toBeVisible();
});

/**
 * A routine used to be an agent preset and nothing else. A workflow is the
 * other thing worth scheduling — "ship the nightly build" is a graph, not one
 * agent — and it takes the same trigger, the same environment and the same
 * prompt, which becomes the run's input.
 */
test("Q5: a routine can run a workflow, and its run is a run", async ({
  page,
  appBase,
  mock,
  apiBase,
}) => {
  await mock.reset();
  await mock.queueToolCall("submit_result", { outcome: "success", description: "shipped" });
  await seedAgent(page, apiBase, "e2e-wf-routine-agent");
  const res = await page.request.post(`${apiBase}/workflows`, {
    data: {
      name: "e2e-routine-workflow",
      description: "from e2e",
      start: "ship",
      steps: [
        { name: "ship", agent: "e2e-wf-routine-agent", prompt: "ship it" },
      ],
    },
  });
  expect([201, 409]).toContain(res.status());
  const before = await sessionCount(page, apiBase);

  await page.goto(`${appBase}/routines`);
  await page.getByTestId("new-routine-button").click();
  await page.getByTestId("routine-name-input").fill("e2e-wf-routine");
  // Which kind first, then which one: a workflow and a preset are both slugs.
  await page.getByTestId("routine-target-workflow").click();
  await page
    .getByTestId("routine-workflow-select")
    .selectOption("e2e-routine-workflow");
  await page.getByTestId("routine-prompt-input").fill("do the release");
  await page.getByTestId("config-environment").click();
  await page.locator('[data-testid="environment-option"][data-value="e2e"]').click();
  await page.keyboard.press("Escape");
  await page.getByTestId("save-routine-button").click();

  await page.waitForURL(
    (url) => url.pathname === projectRoot() + "/routines/e2e-wf-routine",
  );
  await expect(page.getByTestId("routine-detail-page")).toContainText(
    "e2e-routine-workflow",
  );

  // Firing it starts a run, not a plain session — and like every routine's
  // sessions, it stays out of the session list.
  await page.getByTestId("run-routine-button").click();
  await expect(page.getByTestId("routine-run-row")).toHaveCount(1);
  expect(await sessionCount(page, apiBase)).toBe(before);

  // Opening it lands on the run's page: the graph, with the transcript and
  // timeline keys switched off.
  await page.getByTestId("routine-run-row").first().click();
  await page.waitForURL(/\/sessions\/[0-9a-f-]+$/);
  await expect(page.getByTestId("graph-toggle")).toHaveAttribute(
    "aria-checked",
    "true",
  );
  await expect(page.getByTestId("transcript-toggle")).toBeDisabled();

  // A workflow a routine runs cannot be deleted out from under it.
  const del = await page.request.delete(
    `${apiBase}/workflows/e2e-routine-workflow`,
  );
  expect(del.status()).toBe(409);
});

test("Q3: deleting a routine takes its runs with it", async ({
  page,
  appBase,
  apiBase,
}) => {
  await seedAgent(page, apiBase, "e2e-doomed-agent");
  const created = await page.request.post(`${apiBase}/routines`, {
    data: {
      name: "e2e-doomed",
      target: { type: "Agent", value: { agent: "e2e-doomed-agent" } },
      environment: { type: "Runtime", value: { vendor: "e2e" } },
      prompt: "say hello",
    },
  });
  expect(created.status()).toBe(201);
  const run = await page.request.post(
    `${apiBase}/routines/e2e-doomed/run`,
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

  const gone = await page.request.get(`${apiBase}/sessions/${session.id}`);
  expect(gone.status()).toBe(404);
});
