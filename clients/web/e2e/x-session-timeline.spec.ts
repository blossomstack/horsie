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
  await mock.queueToolCall("spawn_agent", { label: "audit", task: "audit the dependencies" });
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

  // The subagent has a lane of its own, and it opens that agent.
  const sub = page.locator('[data-testid^="timeline-lane-"][data-kind="subagent"]').first();
  await expect(sub).toBeVisible();
  await sub.locator('[data-testid^="timeline-span-"]').click();
  await expect(page).toHaveURL(/\/agents\//);

  // ...and there the toggle is gone. Scoped to one agent, the transcript is
  // that agent's while the roster is still the whole session's, so the map
  // would label the open agent "main agent" and hang its siblings off it.
  await expect(page.getByTestId("timeline-toggle")).toHaveCount(0);
  await expect(page.getByTestId("session-timeline")).toHaveCount(0);
});

test("X3: a scoped agent page will not open the timeline, even if the URL asks", async ({
  page,
  appBase,
  mock,
}) => {
  await delegatingSession(page, appBase, mock);

  await page.getByTestId("timeline-toggle").click();
  const sub = page.locator('[data-testid^="timeline-lane-"][data-kind="subagent"]').first();
  await sub.locator('[data-testid^="timeline-span-"]').click();
  await expect(page).toHaveURL(/\/agents\//);

  // `view=timeline` is still on the URL from the toggle — the agent page must
  // ignore it rather than draw the session's map over one agent's transcript.
  await page.goto(`${page.url().split("?")[0]}?view=timeline`);
  await expect(page.getByTestId("transcript-scroll")).toBeVisible();
  await expect(page.getByTestId("session-timeline")).toHaveCount(0);
});

test("X2: a bar goes back to the transcript, and the view lives in the URL", async ({
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

  await page.locator('[data-testid^="timeline-bar-"]').first().click();

  await expect(page.getByTestId("session-timeline")).toHaveCount(0);
  await expect(page.getByTestId("transcript-scroll")).toBeVisible();
  await expect(page).not.toHaveURL(/view=timeline/);
});
