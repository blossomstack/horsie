// Group I — windowed log load. The transcript paints from
// `GET /sessions/:id/messages`, which replays the log from the start and then
// goes live on the same connection. These verify the browser wiring end-to-end:
// a reloaded session repaints, live updates continue afterward, and the
// scroll-back affordance stays hidden for a short transcript.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

/** How many LLM messages a page of the log holds.
 *
 * A page is a list of log *entries*, and not every entry is a message the model
 * saw — a plugin hook record is one, and so is every session lifecycle event.
 * Counting entries would make this test's turn arithmetic depend on how many
 * of those happened to land.
 */
// The window is asked for in entries, and a turn contributes several lifecycle
// entries besides its two messages — so a page sized for "52 messages" has to
// be sized well past 52.
function llmMessageCount(page: unknown): number {
  const entries =
    (page as { entries?: { body?: { type?: string } }[] }).entries ?? [];
  return entries.filter((e) => e.body?.type === "Llm").length;
}


test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("I1: a reloaded session repaints from the log and keeps streaming live", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("first answer");
  await createSession(page, appBase);
  await sendMessage(page, "first question");
  await expect(page.getByTestId("assistant-text")).toContainText("first answer");
  await expectStatus(page, "Idle");

  // Reload: the transcript comes back from the log, which the stream replays
  // from the start — there is no separate read to fall out of step with it.
  await page.reload();
  await expect(
    page.locator('[data-testid="message"][data-role="User"]'),
  ).toContainText("first question");
  await expect(page.getByTestId("assistant-text")).toContainText("first answer");

  // A new turn after the reload proves the same connection that replayed is
  // still delivering live.
  await mock.queueText("second answer");
  await sendMessage(page, "second question");
  await expect(page.getByTestId("assistant-text").last()).toContainText(
    "second answer",
  );
  await expectStatus(page, "Idle");

  // Short transcript → nothing older to load.
  await expect(page.getByTestId("history-load-more")).toHaveCount(0);
});

test("I2: a long session replays whole, so there is nothing to scroll back for", async ({
  page,
  appBase,
  mock,
}) => {
  test.setTimeout(60_000);
  // 26 turns → 52 messages, just past the 50-message window, so the tail omits
  // the oldest turn and scroll-up must fetch it.
  const turns = 26;
  for (let i = 1; i <= turns; i++) await mock.queueText(`answer ${i}`);

  await createSession(page, appBase);
  // Turn 1 through the UI creates the session and yields its id; it consumes
  // the first queued mock reply (`answer 1`). The UI navigates as soon as the
  // message is accepted (202), not once the turn completes, so wait for the
  // same count:status settle the API-driven turns below wait for — otherwise
  // question 2 is queued onto the still-Running turn 1 and the two are merged
  // into a single turn, one message short of the window this test needs.
  const id = await sendMessage(page, "question 1");
  await expect
    .poll(
      async () => {
        const [h, s] = await Promise.all([
          page.request.get(`${appBase}/api/sessions/${id}/messages?aid=main&max=1000`),
          page.request.get(`${appBase}/api/sessions/${id}`),
        ]);
        const count = llmMessageCount(await h.json());
        const status = (await s.json()).session.status as string;
        return `${count}:${status}`;
      },
      { timeout: 15_000 },
    )
    .toBe("2:Idle");

  // Seed the remaining turns over the API (fast + deterministic); each must
  // fully finish before the next (a mid-turn message merges into it). Gate on
  // *both* the reply count reaching 2*i (proving turn i actually ran and
  // produced its answer — which rules out reading the stale pre-Running Idle)
  // and the status settling back to Idle (proving TurnCompleted persisted).
  // Waiting on either alone races one of the two Idle↔Running transitions.
  for (let i = 2; i <= turns; i++) {
    const res = await page.request.post(
      `${appBase}/api/sessions/${id}/messages`,
      { data: { text: `question ${i}` } },
    );
    expect(res.status()).toBe(202);
    await expect
      .poll(
        async () => {
          const [h, s] = await Promise.all([
            page.request.get(`${appBase}/api/sessions/${id}/messages?aid=main&max=1000`),
            page.request.get(`${appBase}/api/sessions/${id}`),
          ]);
          const count = llmMessageCount(await h.json());
          const status = (await s.json()).session.status as string;
          return `${count}:${status}`;
        },
        { timeout: 15_000 },
      )
      .toBe(`${2 * i}:Idle`);
  }

  // A fresh load replays the log from the start and then goes live on the same
  // connection, so the whole transcript is present — oldest turn included.
  //
  // This used to paint a 50-message tail and fetch older pages on scroll-up.
  // The window existed because a read and a subscription were two requests, and
  // the tail was how the client kept the first one small enough to land before
  // the second. With one request there is no seam to keep small: the client
  // asks once, from the beginning.
  //
  // The paging form of the endpoint is still there (`?before=&max=`) and still
  // tested server-side; the web client simply has no reason to reach for it.
  // If replay cost becomes real on a long session, windowing here is the
  // optimization — and it would be a client change alone.
  await page.reload();
  await expect(page.getByTestId("assistant-text").last()).toContainText(
    "answer 26",
  );
  // Assert on the oldest *assistant* text — the user's "question 1" also
  // appears as the session title, so it is not transcript-specific.
  await expect(page.getByText("answer 1", { exact: true })).toBeVisible({
    timeout: 10_000,
  });
  await expect(page.getByTestId("history-load-more")).toHaveCount(0);
});
