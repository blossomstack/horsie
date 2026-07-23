// Group I — windowed history load. The transcript now paints from
// `GET /history` (not an SSE journal replay) and then streams live over a
// live-only SSE connection. These verify the browser wiring end-to-end: a
// reloaded session repaints from history, live updates continue afterward, and
// the scroll-back affordance stays hidden for a short transcript.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("I1: a reloaded session repaints from /history and keeps streaming live", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("first answer");
  await createSession(page, appBase);
  await sendMessage(page, "first question");
  await expect(page.getByTestId("assistant-text")).toContainText("first answer");
  await expectStatus(page, "Idle");

  // Reload: the transcript must come back via the windowed /history load, not a
  // full SSE replay.
  await page.reload();
  await expect(
    page.locator('[data-testid="message"][data-role="User"]'),
  ).toContainText("first question");
  await expect(page.getByTestId("assistant-text")).toContainText("first answer");

  // A new turn after reload proves the live-only SSE stream is delivering
  // events (not relying on replay).
  await mock.queueText("second answer");
  await sendMessage(page, "second question");
  await expect(page.getByTestId("assistant-text").last()).toContainText(
    "second answer",
  );
  await expectStatus(page, "Idle");

  // Short transcript → nothing older to load.
  await expect(page.getByTestId("history-load-more")).toHaveCount(0);
});

test("I2: a long session windows the tail and scroll-up loads older messages", async ({
  page,
  appBase,
  mock,
}) => {
  test.setTimeout(60_000);
  // 26 turns → 52 messages, just past the 50-message window, so the tail omits
  // the oldest turn and scroll-up must fetch it.
  const turns = 26;
  for (let i = 1; i <= turns; i++) await mock.queueText(`answer ${i}`);

  const id = await createSession(page, appBase);

  // Seed the turns over the API (fast + deterministic); each must fully finish
  // before the next (a second message 409s mid-turn). Gate on *both* the reply
  // count reaching 2*i (proving turn i actually ran and produced its answer —
  // which rules out reading the stale pre-Running Idle) and the status settling
  // back to Idle (proving TurnCompleted persisted). Waiting on either alone
  // races one of the two Idle↔Running transitions.
  for (let i = 1; i <= turns; i++) {
    const res = await page.request.post(
      `${appBase}/api/sessions/${id}/messages`,
      { data: { text: `question ${i}` } },
    );
    expect(res.status()).toBe(202);
    await expect
      .poll(
        async () => {
          const [h, s] = await Promise.all([
            page.request.get(`${appBase}/api/sessions/${id}/history?limit=200`),
            page.request.get(`${appBase}/api/sessions/${id}`),
          ]);
          const count = ((await h.json()).messages as unknown[]).length;
          const status = (await s.json()).status as string;
          return `${count}:${status}`;
        },
        { timeout: 15_000 },
      )
      .toBe(`${2 * i}:Idle`);
  }

  // Fresh load → tail window only: newest turn present, oldest absent. Assert
  // on the oldest *assistant* text ("answer 1") — the user's "question 1" also
  // appears as the session title/sidebar row, so it isn't transcript-specific.
  await page.reload();
  await expect(page.getByTestId("assistant-text").last()).toContainText(
    "answer 26",
  );
  await expect(page.getByText("answer 1", { exact: true })).toHaveCount(0);
  await expect(page.getByTestId("history-load-more")).toBeVisible();

  // Scroll to the top to pull the older page; the oldest turn appears.
  const scroller = page.getByTestId("transcript-scroll");
  await scroller.evaluate((el) => (el.scrollTop = 0));
  await expect(page.getByText("answer 1", { exact: true })).toBeVisible({
    timeout: 10_000,
  });
});
