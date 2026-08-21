// Group T — workflows: the editor builds a two-step definition through its
// sidebar, reorders it, visualizes it, and the new-session page runs it.
//
// Every step ends by calling `submit_result`, so a run is two queued tool calls
// rather than two texts: a step that ends a turn with prose has not finished,
// and the server nudges it. T4 adds a step that asks, which is now the ordinary
// `ask_user` tool rather than a second meaning on the finishing one.
import type { Page } from "@playwright/test";
import { expect, test } from "./fixtures";
import { expectStatus, projectRoot } from "./helpers";

const WORKFLOW = "e2e-workflow";
const ASK_WORKFLOW = "e2e-workflow-ask";
const AGENT = "e2e-workflow-agent";

interface WorkflowBody {
  start: string;
  steps: { name: string; agent: string }[];
}

/** The agent preset both steps run as, on the connected vendor. */
async function seedAgent(page: Page, apiBase: string): Promise<void> {
  const cfg = (await (await page.request.get(`${apiBase}/config`)).json()) as {
    models: { alias: string }[];
    vendors: { name: string }[];
  };
  const res = await page.request.post(`${apiBase}/agents`, {
    data: { name: AGENT, model: cfg.models[0]?.alias, vendor: cfg.vendors[0]?.name },
  });
  expect([201, 409]).toContain(res.status());
}

async function fetchWorkflow(page: Page, apiBase: string): Promise<WorkflowBody> {
  const res = await page.request.get(`${apiBase}/workflows/${WORKFLOW}`);
  expect(res.status()).toBe(200);
  return (await res.json()) as WorkflowBody;
}

test("T1: the sidebar links to the workflows page", async ({ page, appBase }) => {
  await page.goto(appBase);
  await page.getByTestId("workflows-link").click();
  await page.waitForURL((url) => url.pathname === projectRoot() + "/workflows");
  await expect(page.getByTestId("workflows-page")).toBeVisible();
});

test("T2: the editor builds, reorders and visualizes a definition", async ({
  page,
  appBase,
  apiBase,
}) => {
  await seedAgent(page, apiBase);
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
  await page.waitForURL((url) => url.pathname === projectRoot() + `/workflows/${WORKFLOW}`);
  await expect(page.getByTestId("workflow-graph")).toBeVisible();

  // The reorder is what was saved, and it did not disturb the start step.
  const saved = await fetchWorkflow(page, apiBase);
  expect(saved.steps.map((s) => s.name)).toEqual(["step-2", "start"]);
  expect(saved.start).toBe("start");
  expect(saved.steps.every((s) => s.agent === AGENT)).toBe(true);
});

test("T3: Run hands the workflow to the new-session page, which starts it", async ({
  page,
  appBase,
  mock,
  apiBase,
}) => {
  await mock.reset();
  // One submission per step: a step ends by calling `submit_result`, and both
  // steps here take the default success/failure outcomes.
  await mock.queueToolCall("submit_result", {
    outcome: "success",
    description: "hello",
  });
  await mock.queueToolCall("submit_result", {
    outcome: "success",
    description: "goodbye",
  });

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
  // A step that is over says so. It used to read `RUNNING` for ever — through
  // reloads and cold tabs — with INTERRUPT still offered, because a step's
  // outcome is journaled as `StepConcluded` rather than as a turn ending, and
  // the client only cleared `Running` on the latter.
  await expectStatus(page, "Idle");
  await expect(page.getByTestId("step-stop")).toHaveCount(0);
  await page.reload();
  await expectStatus(page, "Idle");

  // It landed under the workflow it was started from, and the row says what
  // became of it. This column read a status that only existed while the session
  // was loaded, so every past run showed an em dash and a finished run, a
  // failed one and one parked on a question were indistinguishable.
  await page.goto(`${appBase}/workflows/${WORKFLOW}`);
  await expect(page.getByTestId("workflow-run-row")).toHaveCount(1);
  const runStatus = page
    .getByTestId("workflow-run-row")
    .getByTestId("status-badge");
  await expect(runStatus).toHaveAttribute("data-status", "Finished");
  await expect(runStatus).toHaveText("Finished");
});

/** A one-step workflow whose step may ask. `interactive` is what grants
 * `ask_user`; without it the step has no way to ask at all. */
async function seedAskWorkflow(page: Page, apiBase: string): Promise<void> {
  await seedAgent(page, apiBase);
  const body = {
    name: ASK_WORKFLOW,
    description: "from e2e",
    start: "triage",
    steps: [
      {
        name: "triage",
        agent: AGENT,
        prompt: "triage the report",
        interactive: true,
        outcomes: [
          { value: "p0", description: "drop everything" },
          { value: "p2", description: "file it" },
        ],
      },
    ],
  };
  const res = await page.request.post(`${apiBase}/workflows`, { data: body });
  if (res.status() === 201) return;
  const put = await page.request.put(`${apiBase}/workflows/${ASK_WORKFLOW}`, {
    data: body,
  });
  expect(put.status()).toBe(200);
}

test("T4: a step's question and its answer stand in the step's transcript", async ({
  page,
  appBase,
  mock,
  apiBase,
}) => {
  await mock.reset();
  await seedAskWorkflow(page, apiBase);
  // The step does some work and *then* asks. That is the shape that matters:
  // a question sharing a turn with other tool calls used to be folded into the
  // collapsed "Ran 2 tools" row, so neither it nor the answer was readable.
  await mock.queueToolCall("bash", { command: "echo triaging" });
  await mock.queueToolCall("ask_user", {
    question: "How bad is it?",
    choices: ["p0", "p2"],
  });
  await mock.queueToolCall("submit_result", {
    outcome: "p0",
    description: "It is a p0.",
  });

  await page.goto(`${appBase}/workflows/${ASK_WORKFLOW}`);
  await page.getByTestId("run-workflow").click();
  await page.waitForURL((url) => url.searchParams.get("workflow") === ASK_WORKFLOW);
  const input = page.getByTestId("composer-input");
  await input.fill("the build is red");
  await input.press("Enter");
  await page.waitForURL(/\/sessions\/[0-9a-f-]+$/);

  // The run page says which step is blocked and hands you to it; the question
  // itself lives in that step's own transcript.
  await expect(page.getByTestId("run-status")).toHaveAttribute(
    "data-status",
    "AwaitingInput",
    { timeout: 30_000 },
  );
  await page.getByTestId("open-parked-step").click();
  await page.waitForURL(/\/sessions\/[0-9a-f-]+\/agents\/[0-9a-f-]+$/);

  // Standalone and answerable, with the tool call that preceded it still there.
  await expect(page.getByTestId("ask-user-card")).toContainText("How bad is it?");
  await expect(
    page.locator('[data-testid="tool-call-card"][data-tool="bash"]'),
  ).toBeVisible();

  await page.locator('[data-testid="ask-user-choice"][data-value="p0"]').click();
  await page.getByTestId("ask-user-send").click();

  // And once answered it stays readable: the record that a human was asked
  // something, and what they said.
  await expect(page.getByTestId("ask-user-answer")).toHaveText("p0", {
    timeout: 30_000,
  });
});
