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
  await mock.queueToolCall("spawn_agent", { title: "audit", task: "audit the dependencies" });
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
  // Every node says what kind of thing it is, which is the one claim a shape
  // alone could never make. Scoped to the graph: "subagent" is a word the rail
  // uses too, and an unscoped match is a strict-mode failure waiting for the
  // next component that says it.
  const graph = page.getByTestId("agent-graph");
  await expect(graph.getByText("main session", { exact: true })).toBeVisible();
  await expect(graph.getByText("subagent", { exact: true })).toBeVisible();
  // The subagent is named for the title its spawner gave it.
  await expect(page.getByRole("button", { name: "Show audit" })).toBeVisible();
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

  await fold.click();
  await expect(nodes).toHaveCount(2);
  await expect(page.locator('[data-testid^="agent-hidden-"]')).toHaveCount(0);
});

test("Z3: a node shows the agent beside the graph rather than leaving it", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);
  await page.getByTestId("graph-toggle").click();

  const sub = page.getByRole("button", { name: "Show audit" });
  await sub.click();

  // The panel answers, and the graph is still on screen. Clicking a node used
  // to navigate, which answered "what is this node?" by closing the picture
  // that raised the question.
  await expect(page.getByTestId("agent-panel")).toBeVisible();
  await expect(page.getByTestId("agent-panel-title")).toHaveText("audit");
  await expect(page.getByTestId("agent-panel-input")).toContainText(
    "audit the dependencies",
  );
  await expect(page.getByTestId("agent-graph")).toBeVisible();
  await expect(page).not.toHaveURL(/\/agents\//);
});

/** The id a node's own controls are keyed by. Every node has a jump key now,
 *  so an unscoped `agent-jump-` locator matches the whole graph. */
async function nodeId(node: import("@playwright/test").Locator): Promise<string> {
  const testid = await node.getAttribute("data-testid");
  return (testid ?? "").slice("agent-node-".length);
}

test("Z3b: the jump key on a node opens that agent's transcript", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);
  await page.getByTestId("graph-toggle").click();

  const sub = page.locator('[data-testid^="agent-node-"][data-kind="subagent"]');
  await page.getByTestId(`agent-jump-${await nodeId(sub)}`).click();
  await expect(page).toHaveURL(/\/agents\//);
  // In the transcript, which is what the key says it opens. It used to land in
  // whichever view was remembered — so from the graph it opened the graph,
  // one run over, with the transcript it promised nowhere in sight.
  await expect(page.getByTestId("transcript-scroll")).toBeVisible();
  await expect(page.getByTestId("transcript-toggle")).toHaveAttribute("aria-checked", "true");
  // The switch is on every run now, so a subagent's page has its own way back
  // into either structural view.
  await expect(page.getByTestId("graph-toggle")).toBeVisible();
});

/** The root is a run like any other, and from a subagent's page it is the one
 *  node worth pressing: the way back to the session. It used to be the one
 *  node with no key, on the reasoning that its transcript is the page the
 *  graph is drawn on — true only of the session's own page. */
test("Z3c: the root node marks the run being read, and its key goes back to it", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);
  const sessionUrl = page.url().replace(/\?.*$/, "");

  await page.getByTestId("graph-toggle").click();
  // On the session's own page the root is the run being read.
  const current = page.locator('[data-testid^="agent-node-"][data-current="true"]');
  await expect(current).toHaveAttribute("data-kind", "main");

  const sub = page.locator('[data-testid^="agent-node-"][data-kind="subagent"]');
  await page.getByTestId(`agent-jump-${await nodeId(sub)}`).click();
  await expect(page).toHaveURL(/\/agents\//);

  // The graph is the session's whichever run you are on, and the mark moves.
  await page.getByTestId("graph-toggle").click();
  await expect(current).toHaveAttribute("data-kind", "subagent");

  const root = page.locator('[data-testid^="agent-node-"][data-kind="main"]');
  await page.getByTestId(`agent-jump-${await nodeId(root)}`).click();
  await expect(page).toHaveURL(sessionUrl);
  await expect(page.getByTestId("transcript-scroll")).toBeVisible();
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

  // The three views are answers to the same question, so one takes the pane
  // from the other rather than stacking on it, and the switch shows which one
  // holds it.
  await page.getByTestId("timeline-toggle").click();
  await expect(page.getByTestId("session-timeline")).toBeVisible();
  await expect(page.getByTestId("agent-graph")).toHaveCount(0);
  await expect(page.getByTestId("timeline-toggle")).toHaveAttribute("aria-checked", "true");
  await expect(page.getByTestId("graph-toggle")).toHaveAttribute("aria-checked", "false");

  // The transcript is a setting of the same control, not the absence of one.
  await page.getByTestId("transcript-toggle").click();
  await expect(page.getByTestId("transcript-scroll")).toBeVisible();
  await expect(page.getByTestId("session-timeline")).toHaveCount(0);
  await expect(page).not.toHaveURL(/view=/);
  await expect(page.getByTestId("transcript-toggle")).toHaveAttribute("aria-checked", "true");
});

test("Z5: the view you picked is remembered, and a link still overrides it", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);
  const url = page.url();

  await page.getByTestId("graph-toggle").click();

  // Opening the session with a URL that says nothing about the view: the one
  // picked last is what it opens in, and the URL is corrected to say so — the
  // address bar must not promise a different page than the one on screen.
  await page.goto(url);
  await expect(page.getByTestId("agent-graph")).toBeVisible();
  await expect(page).toHaveURL(/view=graph/);

  // A link that names a view is someone else's choice, and it wins.
  await page.goto(`${url}?view=timeline`);
  await expect(page.getByTestId("session-timeline")).toBeVisible();
  await expect(page.getByTestId("agent-graph")).toHaveCount(0);

  // A session you just started is not one you opened: it is the answer to a
  // message you just typed, so it lands in the transcript however you were
  // working a moment ago. The graph is still what is remembered.
  await mock.queueText("a fresh thread");
  await createSession(page, appBase);
  await sendMessage(page, "start something new");
  await expect(page.getByTestId("transcript-scroll")).toBeVisible();
  await expect(page.getByTestId("agent-graph")).toHaveCount(0);
  await expect(page.getByTestId("transcript-toggle")).toHaveAttribute("aria-checked", "true");
});

/** A sub session is on the graph, and the graph is how you reach one.
 *
 * The rail lists sessions only, so this is the surface that has to work: with
 * sub sessions missing from the roster it laid out, a session made entirely of
 * them drew one node and said it had branched nothing.
 */
test("Z6: a sub session is drawn under the session it branched from, and opens from there", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.reset();
  await mock.queueText("the original answer");
  await mock.queueText("the sub session's answer");

  await createSession(page, appBase);
  await sendMessage(page, "start the migration");
  await expectStatus(page, "Idle");

  // `/fork` redirects to the sub session it just made; the graph is the
  // session's, so go back to it.
  await sendMessage(page, "/fork try the other way");
  await page.waitForURL(/\/agents\/[0-9a-f-]+$/);
  const subSessionUrl = page.url();
  await page.goBack();
  await page.waitForURL(/\/sessions\/[0-9a-f-]+(\?.*)?$/);

  await page.getByTestId("graph-toggle").click();
  await expect(page.getByTestId("agent-graph")).toBeVisible();
  await expect(page.locator('[data-testid^="agent-node-"]')).toHaveCount(2);
  const branched = page.locator('[data-testid^="agent-node-"][data-kind="sub_session"]');
  await expect(branched).toHaveCount(1);

  // Selecting shows it beside the graph; the jump key is what leaves.
  await branched.click();
  await expect(page.getByTestId("agent-panel")).toBeVisible();
  await expect(page.getByTestId("agent-panel-readout")).toHaveText("sub session");

  await page.getByTestId(`agent-jump-${await nodeId(branched)}`).click();
  await expect(page).toHaveURL(subSessionUrl.replace(/\?.*$/, ""));
});

/** The composer belongs to the transcript.
 *
 * Under a picture of the session it was an input wired to something the reader
 * was not looking at. */
test("Z7: the timeline and the graph have no composer under them", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);

  await expect(page.getByTestId("composer-input")).toBeVisible();

  await page.getByTestId("graph-toggle").click();
  await expect(page.getByTestId("agent-graph")).toBeVisible();
  await expect(page.getByTestId("composer-input")).toHaveCount(0);
  await expect(page.getByTestId("session-config-bar")).toHaveCount(0);

  await page.getByTestId("timeline-toggle").click();
  await expect(page.getByTestId("session-timeline")).toBeVisible();
  await expect(page.getByTestId("composer-input")).toHaveCount(0);

  // And it comes back with the conversation.
  await page.getByTestId("transcript-toggle").click();
  await expect(page.getByTestId("composer-input")).toBeVisible();
});
