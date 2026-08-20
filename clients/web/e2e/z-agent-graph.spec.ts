// Group Z — the agent graph: the session's lineage instead of its prose.
//
// Group X's setup, for the same reason it uses group S's: a session that
// delegates is the one worth drawing. The ordering note there applies here too
// — the main agent's continuation and the subagent's only call race for the
// mock's single FIFO, so both racers are given the same text.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

const RACING = "working on it";
const FINAL = "the audit is in";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

/** A settled session that spawned one subagent, ready to be drawn. */
async function delegatingSession(
  page: import("@playwright/test").Page,
  appBase: string,
  mock: {
    queueToolCall: (name: string, input: unknown) => Promise<void>;
    queueText: (text: string) => Promise<void>;
  },
) {
  await mock.queueToolCall("spawn_agent", { label: "audit", task: "audit the dependencies" });
  await mock.queueText(RACING);
  await mock.queueText(RACING);
  await mock.queueText(FINAL);

  await createSession(page, appBase);
  await sendMessage(page, "delegate the dependency audit");

  // Settled before anything is drawn: a graph built mid-turn is a different
  // picture, and asserting on it would be asserting on a race.
  await expect(page.locator('[data-testid="subagent-card"]')).toBeVisible();
  await expect(page.getByTestId("assistant-text").last()).toContainText(FINAL);
  await expectStatus(page, "Idle");
}

test("Z1: the graph draws the main agent and what it spawned", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);

  await page.getByTestId("graph-toggle").click();
  await expect(page.getByTestId("agent-graph")).toBeVisible();
  // The transcript gives up the pane rather than sharing it.
  await expect(page.getByTestId("session-timeline")).toHaveCount(0);

  // Two nodes: the main agent, and the agent it spawned. Matched on structure
  // rather than on text — every node carries its name three times over, in the
  // node, in its hover title and in its fold's label.
  await expect(page.locator('[data-testid^="agent-node-"]')).toHaveCount(2);
  await expect(page.getByText("main agent", { exact: true })).toBeVisible();
  // The subagent is the one that can be opened, and it is named for its task.
  await expect(page.getByRole("button", { name: "Open audit" })).toBeVisible();
});

test("Z2: folding an agent hides what it spawned, and unfolding brings it back", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);
  await page.getByTestId("graph-toggle").click();

  const nodes = page.locator('[data-testid^="agent-node-"]');
  await expect(nodes).toHaveCount(2);

  // The main agent's fold is the only one on screen — the subagent spawned
  // nothing, so it has no control at all.
  const fold = page.locator('[data-testid^="agent-collapse-"]');
  await expect(fold).toHaveCount(1);
  await expect(fold).toHaveAttribute("aria-expanded", "true");

  await fold.click();
  await expect(nodes).toHaveCount(1);
  await expect(fold).toHaveAttribute("aria-expanded", "false");
  // What the fold stands for is on the node, so the count is not lost with it.
  await expect(page.locator('[data-testid^="agent-hidden-"]')).toHaveText("+1");
  // A folded session still spawned what it spawned. Keyed on what was drawn,
  // this told a session with folded subagents that it had never had any.
  await expect(page.getByTestId("agent-graph-lonely")).toHaveCount(0);

  await fold.click();
  await expect(nodes).toHaveCount(2);
  await expect(page.locator('[data-testid^="agent-hidden-"]')).toHaveCount(0);
});

test("Z3: a node opens the agent it names", async ({ page, appBase, mock }) => {
  await delegatingSession(page, appBase, mock);
  await page.getByTestId("graph-toggle").click();

  // The main agent's node is not a way in: its transcript is the page the
  // graph is drawn on.
  const sub = page.locator('[data-testid^="agent-node-"][role="button"]');
  await expect(sub).toHaveCount(1);
  await sub.click();
  await expect(page).toHaveURL(/\/agents\//);

  // ...and there the toggle is gone, for the reason group X pins: scoped to one
  // agent, the roster is still the whole session's, so the graph would label
  // the open agent "main agent" and hang its siblings off it.
  await expect(page.getByTestId("graph-toggle")).toHaveCount(0);
  await expect(page.getByTestId("agent-graph")).toHaveCount(0);
});

test("Z4: the view is in the URL, so it survives a reload and can be sent", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);

  await page.getByTestId("graph-toggle").click();
  await expect(page).toHaveURL(/view=graph/);
  await page.reload();
  await expect(page.getByTestId("agent-graph")).toBeVisible();

  // The two structural views are answers to the same question, so one takes
  // the pane from the other rather than stacking on it.
  await page.getByTestId("timeline-toggle").click();
  await expect(page.getByTestId("session-timeline")).toBeVisible();
  await expect(page.getByTestId("agent-graph")).toHaveCount(0);
});
