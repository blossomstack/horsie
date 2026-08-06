// Group T — plugin hook records reach the transcript and SURVIVE A RELOAD.
//
// The reload is the whole point. Every other assertion here would pass against
// an ephemeral SSE frame; only re-fetching `/history` proves the records were
// journaled, which is the reason they are journaled at all.
//
// The fixture is the shared plugin library global-setup already builds: one
// `SessionStart` hook (a record with no tool call → its own transcript row) and
// one `PostToolUse` hook (a record with a `ToolScope` → attached to its card).

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("T1: hook records render, tool-scoped and standalone, and survive a reload", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("bash", { command: "echo E2E_HOOKED" });
  await mock.queueText("Done.");
  await createSession(page, appBase, { skills: ["e2e-plugin"] });
  await sendMessage(page, "run the tool");
  await expectStatus(page, "Idle");

  // A `SessionStart` record has no call to attach to, so it is a row of its
  // own — the rendering that did not exist before this change, and the record
  // that did not exist before it either.
  const notice = page.getByTestId("hook-notice");
  await expect(notice).toContainText("e2e-plugin");
  await expect(notice).toContainText("SessionStart");

  const card = page.locator('[data-testid="tool-call-card"][data-tool="bash"]');
  await card.getByTestId("tool-call-toggle").click();
  // `systemMessage` is addressed to the user, not the model: it must appear
  // here and never in the tool output the agent read.
  await expect(card.getByTestId("tool-call-hook")).toContainText("E2E_HOOK_NOTE");
  await expect(card.getByTestId("tool-call-output")).not.toContainText(
    "E2E_HOOK_NOTE",
  );

  await page.reload();

  await expect(page.getByTestId("hook-notice")).toContainText("SessionStart");
  const reloaded = page.locator(
    '[data-testid="tool-call-card"][data-tool="bash"]',
  );
  await reloaded.getByTestId("tool-call-toggle").click();
  await expect(reloaded.getByTestId("tool-call-hook")).toContainText(
    "E2E_HOOK_NOTE",
  );
});
