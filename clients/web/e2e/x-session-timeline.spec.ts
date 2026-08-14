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
