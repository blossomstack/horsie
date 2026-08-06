// Group F — the runtime's setup commands (ScanWorkspace, SessionStart) load
// context INTO THE AGENT'S SYSTEM PROMPT. Only the LLM is doubled, so we assert
// on what the real server actually sent it: the mock captures every request and
// `capturedContains` substring-matches the composed system prompt.
//
// Fixtures are seeded once in global-setup: an AGENTS.md + a workspace skill in
// the scratch workspace, and a --plugins-dir plugin providing a shared skill and
// a SessionStart hook. With the local `e2e` vendor use_plugins defaults to true,
// so all three sources are scanned on agent spawn.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

/**
 * Drive one completed text turn, then return the mock's captured requests.
 * `skills` selects bundles on the draft; the groups that do not assert on
 * plugin content leave it empty so their runtime has nothing to fetch.
 */
async function runTurnAndCapture(
  page: import("@playwright/test").Page,
  appBase: string,
  mock: import("./fixtures").MockLlm,
  skills: string[] = [],
): Promise<void> {
  await mock.queueText("Understood.");
  await createSession(page, appBase, { skills });
  await sendMessage(page, "hello");
  await expect(page.getByTestId("assistant-text")).toContainText("Understood.");
  await expectStatus(page, "Idle");
}

test("F1: workspace AGENTS.md + workspace skill load into the system prompt", async ({
  page,
  appBase,
  mock,
}) => {
  await runTurnAndCapture(page, appBase, mock);
  // AGENTS.md is inlined verbatim; the workspace skill contributes a listing line
  // carrying its directory, relative to the workspace root in the header above it.
  expect(await mock.capturedContains("E2E_AGENTS_MARKER")).toBe(true);
  expect(
    await mock.capturedContains("- e2e-skill — .claude/skills/e2e-skill/: E2E_SKILL_DESC"),
  ).toBe(true);
});

test("F1b: the prompt states the working directory instead of a workspace argument", async ({
  page,
  appBase,
  mock,
}) => {
  await runTurnAndCapture(page, appBase, mock);
  // The tools take no `workspace` argument, so the block says where relative
  // paths land rather than how to name a root.
  expect(await mock.capturedContains("Your working directory starts at ")).toBe(true);
  expect(await mock.capturedContains("`workspace` argument")).toBe(false);
});

test("F2: shared plugin-library skill loads into the system prompt", async ({
  page,
  appBase,
  mock,
}) => {
  await runTurnAndCapture(page, appBase, mock, ["e2e-plugin"]);
  // The library is not a workspace, so its header carries the absolute root the
  // per-skill relative paths hang off — the agent's only handle on those files.
  expect(await mock.capturedContains("# Shared skills — ")).toBe(true);
  expect(
    await mock.capturedContains(
      "- e2e-shared-skill — e2e-plugin/skills/e2e-shared-skill/: E2E_SHARED_DESC",
    ),
  ).toBe(true);
});

// A `SessionStart` hook's context used to be prepended to the system prompt as a
// "# Session bootstrap" section. It is a hook record now, translated into a
// framed message at its place in the transcript — so this asserts on the frame
// as well as the marker, because the frame is what tells the model the text came
// from a third-party plugin rather than from horsie.
test("F3: SessionStart hook output reaches the model as framed context", async ({
  page,
  appBase,
  mock,
}) => {
  await runTurnAndCapture(page, appBase, mock, ["e2e-plugin"]);
  expect(await mock.capturedContains("E2E_BOOTSTRAP_MARKER")).toBe(true);
  // No quotes in the needle: `capturedContains` matches against
  // `JSON.stringify(body)`, where the frame's own quotes are backslash-escaped.
  expect(await mock.capturedContains("<hook-context plugin=")).toBe(true);
  expect(await mock.capturedContains("# Session bootstrap")).toBe(false);
});
