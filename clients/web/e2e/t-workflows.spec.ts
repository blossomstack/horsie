// Group T — workflows: the editor builds a two-step definition through its
// sidebar, reorders it, visualizes it, and the new-session page runs it.
//
// The steps deliberately declare no output schema. Such a step has no
// `conclude` tool and ends its turn with plain text, which becomes its output —
// so a run is two ordinary queued texts rather than a hand-built tool call.
import type { Page } from "@playwright/test";
import { expect, test } from "./fixtures";

const WORKFLOW = "e2e-workflow";
const AGENT = "e2e-workflow-agent";

interface WorkflowBody {
  start: string;
  steps: { name: string; agent: string }[];
}

/** The agent preset both steps run as, on the connected vendor. */
async function seedAgent(page: Page, appBase: string): Promise<void> {
  const cfg = (await (await page.request.get(`${appBase}/api/config`)).json()) as {
    models: { alias: string }[];
    vendors: { name: string }[];
  };
  const res = await page.request.post(`${appBase}/api/agents`, {
    data: { name: AGENT, model: cfg.models[0]?.alias, vendor: cfg.vendors[0]?.name },
  });
  expect([201, 409]).toContain(res.status());
}

async function fetchWorkflow(page: Page, appBase: string): Promise<WorkflowBody> {
  const res = await page.request.get(`${appBase}/api/workflows/${WORKFLOW}`);
  expect(res.status()).toBe(200);
  return (await res.json()) as WorkflowBody;
}

test("T1: the sidebar links to the workflows page", async ({ page, appBase }) => {
  await page.goto(appBase);
  await page.getByTestId("workflows-link").click();
  await page.waitForURL((url) => url.pathname === "/workflows");
  await expect(page.getByTestId("workflows-page")).toBeVisible();
});

test("T2: the editor builds, reorders and visualizes a definition", async ({
  page,
  appBase,
}) => {
  await seedAgent(page, appBase);
  await page.goto(`${appBase}/workflows`);
  await page.getByTestId("new-workflow-button").click();
  await expect(page.getByTestId("workflow-edit-page")).toBeVisible();

  // The definition is what a new workflow opens on.
  await expect(page.getByTestId("definition-form")).toBeVisible();
  await page.getByTestId("workflow-name").fill(WORKFLOW);
  await page.getByTestId("workflow-description").fill("from e2e");

  // The first step, reached from the sidebar.
  await page.getByTestId("select-step").first().click();
  await expect(page.getByTestId("step-form")).toHaveAttribute("data-step-name", "start");
  await page.getByTestId("step-agent").selectOption(AGENT);
  await page.getByTestId("step-prompt").fill("say hello");

  // Adding a step opens it, so the panel follows what was just created.
  await page.getByTestId("add-step").click();
  await expect(page.getByTestId("step-form")).toHaveAttribute("data-step-name", "step-2");
  await page.getByTestId("step-agent").selectOption(AGENT);
  await page.getByTestId("step-prompt").fill("say goodbye");

  // A catch-all transition from the first step to the second.
  await page.getByTestId("select-step").first().click();
  await page.getByTestId("add-transition").click();
  await page.getByTestId("transition-target").selectOption("step-2");

  // The graph takes the whole panel, and choosing a node opens that step —
  // which is what the preview pinned in a gutter could never do.
  await page.getByTestId("visualize-workflow").click();
  await expect(page.getByTestId("workflow-visual")).toBeVisible();
  await expect(page.getByTestId("workflow-node-start")).toBeVisible();
  await page.getByTestId("workflow-node-step-2").click();
  await expect(page.getByTestId("step-form")).toHaveAttribute("data-step-name", "step-2");

  // Reordering from the keyboard: native drag is mouse-only, and this handle
  // is the only way to change the order without one.
  const rows = page.getByTestId("step-row");
  await expect(rows.first()).toHaveAttribute("data-step-name", "start");
  await page.getByTestId("step-handle").nth(1).press("ArrowUp");
  await expect(rows.nth(0)).toHaveAttribute("data-step-name", "step-2");
  await expect(rows.nth(1)).toHaveAttribute("data-step-name", "start");

  await page.getByTestId("save-workflow").click();
  await page.waitForURL((url) => url.pathname === `/workflows/${WORKFLOW}`);
  await expect(page.getByTestId("workflow-graph")).toBeVisible();

  // The reorder is what was saved, and it did not disturb the start step.
  const saved = await fetchWorkflow(page, appBase);
  expect(saved.steps.map((s) => s.name)).toEqual(["step-2", "start"]);
  expect(saved.start).toBe("start");
  expect(saved.steps.every((s) => s.agent === AGENT)).toBe(true);
});

test("T3: Run hands the workflow to the new-session page, which starts it", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.reset();
  // One text per step: with no output schema, a step's turn ends with text and
  // that text is its output.
  await mock.queueText("hello");
  await mock.queueText("goodbye");

  await page.goto(`${appBase}/workflows/${WORKFLOW}`);
  await page.getByTestId("run-workflow").click();
  await page.waitForURL((url) => url.searchParams.get("workflow") === WORKFLOW);

  // The workflow is selected, and the channels a run cannot carry are gone:
  // its model and toolbox come from each step's own preset. A config key is
  // icon-only, so its value is in the accessible name rather than in text.
  await expect(page.getByTestId("config-workflow")).toHaveAttribute(
    "aria-label",
    new RegExp(`Workflow.*${WORKFLOW}`),
  );
  await expect(page.getByTestId("config-environment")).toBeVisible();
  await expect(page.getByTestId("config-model")).toHaveCount(0);

  const input = page.getByTestId("composer-input");
  await input.fill("run it");
  await input.press("Enter");
  await page.waitForURL(/\/sessions\/[0-9a-f-]+$/);

  // A run opens on its graph rather than on a transcript.
  await expect(page.getByTestId("workflow-run-view")).toBeVisible();
  await expect(page.getByTestId("workflow-node-start")).toBeVisible();
  await expect(page.getByTestId("run-status")).toBeVisible();

  // It runs to completion by itself: creating it is what starts it, and each
  // step's plain text is its output.
  await expect(page.getByTestId("run-status")).toHaveAttribute(
    "data-status",
    "Finished",
    { timeout: 30_000 },
  );
  // The result. It was on the wire from the first release and rendered nowhere,
  // so the one thing a finished run produced was reachable only by opening its
  // last step.
  await expect(page.getByTestId("run-output")).toContainText("goodbye");

  // A step's own page is reached from its node, and that is where its
  // transcript is.
  await page.getByTestId("workflow-node-start").click();
  await page.getByTestId("open-step").first().click();
  await page.waitForURL(/\/sessions\/[0-9a-f-]+\/agents\/[0-9a-f-]+$/);
  await expect(page.getByTestId("step-stop")).toBeVisible();

  // It landed under the workflow it was started from.
  await page.goto(`${appBase}/workflows/${WORKFLOW}`);
  await expect(page.getByTestId("workflow-run-row")).toHaveCount(1);
});
