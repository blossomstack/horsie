// Group L — token usage is reported as it is spent, not banked until the turn
// ends. A tool loop can run for a long time between turn boundaries, and a
// stopped turn never reaches a turn boundary at all — the readout has to
// account for both.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

/**
 * The chip reads "<n> tok"; these totals stay small enough not to abbreviate.
 * Short timeout on purpose: the point is that usage shows up *while the tool is
 * still running*, and a retrying assertion with a generous timeout would
 * happily wait out the tool and prove nothing.
 */
const MID_RUN_TIMEOUT = 4_000;

test("L1: a stopped tool loop still reports the tokens it spent", async ({
  page,
  appBase,
  mock,
}) => {
  // One LLM call (paid for), then a tool that outlives the whole assertion
  // budget — so anything asserted below is asserted strictly mid-run.
  await mock.queueToolCall("bash", { command: "sleep 60" });
  await createSession(page, appBase);
  await sendMessage(page, "start a long task");
  await expectStatus(page, "Running");

  const chip = page.getByTestId("context-stats-button");
  const chipTokens = async () => Number((await chip.innerText()).replace(/[^0-9]/g, ""));

  // Live event stream: the first call's cost, while the run is still going.
  await expect(chip).not.toHaveText("0 tok", { timeout: MID_RUN_TIMEOUT });
  const midRun = await chipTokens();
  expect(midRun).toBeGreaterThan(0);

  // The server's own `GET /usage` aggregate agrees. Its "This turn" section
  // renders only when the main agent has a per-run figure to show, which on a
  // first turn still in flight exists only because usage is recorded per
  // provider call rather than at the end of the run.
  await chip.click();
  const panel = page.getByTestId("context-stats-panel");
  await expect(panel).toBeVisible({ timeout: MID_RUN_TIMEOUT });
  await expect(panel).toContainText("This turn", { timeout: MID_RUN_TIMEOUT });
  await expect(panel).toContainText("Session total");
  // Still mid-run: nothing above was satisfied by the turn quietly finishing.
  await expectStatus(page, "Running");

  // Stop: this run never reaches a completion, so its tokens survive only
  // because they were recorded as they were spent.
  await page.getByTestId("composer-stop").click();
  await expectStatus(page, "Stopped");
  expect(await chipTokens()).toBeGreaterThanOrEqual(midRun);

  // The next turn accumulates on top of the stopped one rather than replacing it.
  await mock.queueText("Reattached and finished.");
  await sendMessage(page, "continue");
  await expect(page.getByTestId("assistant-text")).toContainText(
    "Reattached and finished.",
  );
  await expectStatus(page, "Idle");
  expect(await chipTokens()).toBeGreaterThan(midRun);
});
