// Group H — the `task_list` tool's side widget. The tool executes by `ask`ing
// the owning agent actor (never the sandboxed runtime), so it behaves like
// any other tool call from the mock LLM's perspective: queue a `tool_call`
// response, then a follow-up `text` response to end the turn.

import { test, expect, type Page } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

/** The plan panel is closed by default and its visibility is a stored user
 * choice, so a test that wants to read the list opens it first. */
async function openPlan(page: Page) {
  const panel = page.getByTestId("task-list-panel");
  if (await panel.isVisible().catch(() => false)) return panel;
  await page.getByTestId("task-list-toggle").click();
  await expect(panel).toBeVisible();
  return panel;
}

test("H1: no widget until the agent has created a task list", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("Hi there.");
  await createSession(page, appBase);

  await sendMessage(page, "hello");

  await expect(page.getByTestId("assistant-text")).toContainText("Hi there.");
  // The panel is reachable on every session now, so "no plan" is an empty
  // state rather than a missing component — and the header key stays unlit
  // until there is a plan behind it.
  await expect(page.getByTestId("task-list-panel")).toHaveCount(0);
  await expect(page.getByTestId("task-list-toggle")).not.toHaveAttribute(
    "data-has-plan",
    /.+/,
  );
  await openPlan(page);
  await expect(page.getByTestId("task-list-empty")).toBeVisible();
});

test("H2: creating a task list shows it in the side widget", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("task_list", {
    action: "create",
    tasks: ["Set up project", "Implement feature", "Write tests"],
  });
  await mock.queueText("Plan created.");
  await createSession(page, appBase);

  await sendMessage(page, "make a plan");

  // The header key lights once a plan exists; the figures live in the panel
  // and the tooltip, not in a badge on a 2rem key.
  const toggle = page.getByTestId("task-list-toggle");
  await expect(toggle).toHaveAttribute("data-has-plan", "true");
  await expect(toggle).toHaveAttribute("title", /0\/3 done/);
  const panel = await openPlan(page);
  await expect(panel.getByTestId("task-list-progress")).toHaveText("0/3 done");
  await expect(panel.getByTestId("task-list-item")).toHaveCount(3);
  await expect(panel.getByTestId("task-list-item").nth(1)).toContainText(
    "Implement feature",
  );
  await expectStatus(page, "Idle");
});

test("H3: marking a task completed updates the widget", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("task_list", {
    action: "create",
    tasks: ["Step one", "Step two"],
  });
  await mock.queueText("Plan created.");
  await createSession(page, appBase);
  await sendMessage(page, "make a plan");
  await openPlan(page);
  await expectStatus(page, "Idle");

  await mock.queueToolCall("task_list", {
    action: "update_status",
    ids: [1],
    status: "completed",
  });
  await mock.queueText("Finished step one.");
  await sendMessage(page, "mark the first step done");

  const panel = page.getByTestId("task-list-panel");
  await expect(panel.getByTestId("task-list-progress")).toHaveText("1/2 done");
  const first = panel.getByTestId("task-list-item").nth(0);
  await expect(first).toHaveAttribute("data-status", "Completed");
});

test("H4: the plan hides and re-opens from the session header", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("task_list", {
    action: "create",
    tasks: ["Only task"],
  });
  await mock.queueText("Done planning.");
  await createSession(page, appBase);
  await sendMessage(page, "make a plan");

  const panel = await openPlan(page);
  await expect(panel).toBeVisible();

  // Closing from inside the panel and re-opening from the header are the same
  // stored preference seen from two places.
  await panel.getByTestId("task-list-collapse").click();
  await expect(page.getByTestId("task-list-panel")).toHaveCount(0);
  await expect(page.getByTestId("task-list-toggle")).toHaveAttribute(
    "data-has-plan",
    "true",
  );

  await page.getByTestId("task-list-toggle").click();
  await expect(page.getByTestId("task-list-panel")).toBeVisible();

  // And it survives a reload, which is the point of storing it.
  await page.reload();
  await expect(page.getByTestId("task-list-panel")).toBeVisible();
});

test("H5: the plan updates while the turn is still running", async ({
  page,
  appBase,
  mock,
}) => {
  // The panel used to catch up only at a turn boundary, because the list
  // reached it through the agent document and nothing re-read that document
  // mid-turn. A plan is written to be watched *while* the agent works, so the
  // list now rides the agent's log like everything else the transcript folds.
  await mock.queueToolCall("task_list", {
    action: "create",
    tasks: ["First step", "Second step"],
  });
  // The answer that would end the turn is held back, so what follows is read
  // while the turn is genuinely in flight.
  await mock.queueText("Plan created.", 8000);
  await createSession(page, appBase);
  await sendMessage(page, "make a plan");

  const panel = await openPlan(page);
  await expectStatus(page, "Running");
  // Bounded well inside the held answer on purpose: with the suite's 30s
  // default this would simply wait for the turn to end, which is exactly the
  // behaviour being ruled out.
  await expect(panel.getByTestId("task-list-item")).toHaveCount(2, {
    timeout: 4000,
  });
  await expect(panel.getByTestId("task-list-progress")).toHaveText("0/2 done", {
    timeout: 4000,
  });
  await expectStatus(page, "Running");

  // And the held answer still lands, so the case leaves the queue empty.
  await expectStatus(page, "Idle");
});
