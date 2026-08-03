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
  // state rather than a missing component — and the header key carries no
  // progress badge until there is progress to report.
  await expect(page.getByTestId("task-list-panel")).toHaveCount(0);
  await expect(page.getByTestId("task-list-badge")).toHaveCount(0);
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

  await expect(page.getByTestId("task-list-badge")).toHaveText("0/3");
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
  await expect(page.getByTestId("task-list-badge")).toHaveText("0/1");

  await page.getByTestId("task-list-toggle").click();
  await expect(page.getByTestId("task-list-panel")).toBeVisible();

  // And it survives a reload, which is the point of storing it.
  await page.reload();
  await expect(page.getByTestId("task-list-panel")).toBeVisible();
});
