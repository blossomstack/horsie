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
