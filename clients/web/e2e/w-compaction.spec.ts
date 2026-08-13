import { expect, test } from "./fixtures";
import { createSession, sendMessage } from "./helpers";

/** Compaction, as a reader sees it.
 *
 * The requirement the feature exists for is that nothing a person can read is
 * ever removed — so the assertion that matters is not that a boundary appeared
 * but that the messages above it are still there. */
test("W1: a compacted session shows the boundary and keeps its history", async ({
  page,
  appBase,
}) => {
  await createSession(page, appBase, { model: "tiny-window" });
  await sendMessage(page, "the first thing I asked");
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the first thing I asked",
  );

  // The second turn crosses the budget the first one left behind.
  await sendMessage(page, "the second thing I asked");
  const divider = page.getByTestId("compaction-divider");
  await expect(divider).toBeVisible({ timeout: 30_000 });

  // Everything said before the boundary is still on the page.
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the first thing I asked",
  );

  // Collapsed by default; the summary is there when asked for.
  await expect(page.getByTestId("compaction-detail")).toHaveCount(0);
  await page.getByTestId("compaction-toggle").click();
  await expect(page.getByTestId("compaction-detail")).toBeVisible();

  // One boundary, one tick.
  await expect(page.getByTestId("spine-tick")).toHaveCount(1);
});

/** `/compact` on a session that has plenty of room.
 *
 * The reported bug: typing it produced nothing at all — no boundary, no notice,
 * no error. Not folding is the right call here (the default model's 200k window
 * gives a 40k retain budget, and this session is a few hundred tokens), but the
 * command still has to be answered. */
test("W2: /compact with nothing to fold says so and folds nothing", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.reset();
  await mock.queueText("an answer to the first thing");
  await mock.queueText("an answer to the second thing");

  await createSession(page, appBase);
  await sendMessage(page, "the first thing I asked");
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "an answer to the first thing",
  );
  await sendMessage(page, "the second thing I asked");
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "an answer to the second thing",
  );

  // The trailing space matters: without it the text is a half-typed invocation
  // and the command menu owns Enter, so the first press would pick rather than
  // send. A space closes the menu, which is the composer's own rule.
  await sendMessage(page, "/compact ");

  const notice = page.getByTestId("compaction-notice");
  await expect(notice).toBeVisible();
  await expect(notice).toContainText(/nothing to compact/i);

  // Nothing moved: no boundary, and the history it declined to fold is still
  // on the page.
  await expect(page.getByTestId("compaction-divider")).toHaveCount(0);
  await expect(page.getByTestId("transcript-scroll")).toContainText(
    "the first thing I asked",
  );
  // A builtin is never shown to the model as a prompt, and the queue held
  // exactly two answers — a third call would have found it empty.
  expect(await mock.capturedContains("/compact")).toBe(false);
});
