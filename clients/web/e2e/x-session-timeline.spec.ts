// Group X — the timeline view: the session's shape instead of its prose.
//
// The setup is group S's, because a session that delegates is the one worth
// drawing: it has a main lane with bars and a subagent lane hanging off it.
// The ordering note there applies here too — the main agent's continuation and
// the subagent's only call race for the mock's single FIFO, so both racers are
// given the same text.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

const RACING = "working on it";
const FINAL = "the audit is in";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

/** A settled session that spawned one subagent, ready to be drawn. */
async function delegatingSession(page: import("@playwright/test").Page, appBase: string, mock: {
  queueToolCall: (name: string, input: unknown) => Promise<void>;
  queueText: (text: string) => Promise<void>;
}) {
  await mock.queueToolCall("spawn_agent", { title: "audit", task: "audit the dependencies" });
  await mock.queueText(RACING);
  await mock.queueText(RACING);
  await mock.queueText(FINAL);

  await createSession(page, appBase);
  await sendMessage(page, "delegate the dependency audit");

  // Settled before anything is drawn: a timeline built mid-turn is a different
  // picture, and asserting on it would be asserting on a race.
  await expect(page.locator('[data-testid="subagent-card"]')).toBeVisible();
  await expect(page.getByTestId("assistant-text").last()).toContainText(FINAL);
  await expectStatus(page, "Idle");
}

test("X1: the timeline draws the session and clicks through to the subagent", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);

  await page.getByTestId("timeline-toggle").click();
  await expect(page.getByTestId("session-timeline")).toBeVisible();

  // One main lane, carrying bars.
  const main = page.locator('[data-testid^="timeline-lane-"][data-kind="main"]');
  await expect(main).toHaveCount(1);
  await expect(page.locator('[data-testid^="timeline-bar-"]').first()).toBeVisible();

  // The subagent has a lane of its own. Its name in the sidebar is what opens
  // it — the span beside the name drills into its work without leaving.
  const sub = page.locator('[data-testid^="timeline-lane-"][data-kind="subagent"]').first();
  await expect(sub).toBeVisible();
  await sub.locator('[data-testid^="timeline-span-"]').click();
  await expect(sub).toHaveAttribute("data-expanded", "true");
  await expect(page).not.toHaveURL(/\/agents\//);

  // The name shows what the agent is, beside the picture.
  await sub.locator('[data-testid^="timeline-select-"]').click();
  await expect(page.getByTestId("agent-panel")).toBeVisible();
  await expect(page.getByTestId("agent-panel-title")).toHaveText("audit");
  await expect(page).not.toHaveURL(/\/agents\//);

  // The jump key is what leaves.
  await sub.locator('[data-testid^="timeline-open-"]').click();
  await expect(page).toHaveURL(/\/agents\//);

  // ...and there the switch is still offered: the timeline is now drawn of
  // whichever run the page is on, so a subagent's page has one of its own.
  await expect(page.getByTestId("timeline-toggle")).toBeVisible();
});

/** A scoped page draws its *own* run, which is what makes the view offerable
 *  there at all. It used to be refused: the timeline was always the main
 *  agent's, so on a subagent's page it drew the wrong session over the right
 *  transcript. Now the root lane is the run you are on. */
test("X3: a scoped agent page draws the timeline of that run", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);

  await page.getByTestId("timeline-toggle").click();
  const sub = page.locator('[data-testid^="timeline-lane-"][data-kind="subagent"]').first();
  await sub.locator('[data-testid^="timeline-open-"]').click();
  await expect(page).toHaveURL(/\/agents\//);

  await page.getByTestId("timeline-toggle").click();
  await expect(page.getByTestId("session-timeline")).toBeVisible();
  // One lane: this run's. The main agent is above it, not below it.
  const lanes = page.locator('[data-testid^="timeline-lane-"]');
  await expect(lanes).toHaveCount(1);
  await expect(lanes.first()).toHaveAttribute("data-kind", "subagent");
});

test("X2: a bar reads in the panel, and the view lives in the URL", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);

  await page.getByTestId("timeline-toggle").click();
  await expect(page).toHaveURL(/view=timeline/);
  // Survives a reload, which is the whole reason it is in the URL.
  await page.reload();
  await expect(page.getByTestId("session-timeline")).toBeVisible();

  // A bar says what it was, beside the picture. It used to switch straight
  // back to the transcript, which answered "what is this bar?" by closing the
  // timeline that raised the question.
  await page.locator('[data-testid^="timeline-bar-"]').first().click();
  await expect(page.getByTestId("entry-panel")).toBeVisible();
  await expect(page.getByTestId("session-timeline")).toBeVisible();
  await expect(page).toHaveURL(/view=timeline/);

  // Reading it in place is the panel's own key, and that is what leaves.
  await page.getByTestId("entry-panel-open").click();
  await expect(page.getByTestId("session-timeline")).toHaveCount(0);
  await expect(page.getByTestId("transcript-scroll")).toBeVisible();
  await expect(page).not.toHaveURL(/view=timeline/);
});
