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

/** Drive one completed text turn, then return the mock's captured requests. */
async function runTurnAndCapture(
  page: import("@playwright/test").Page,
  appBase: string,
  mock: import("./fixtures").MockLlm,
): Promise<void> {
  await mock.queueText("Understood.");
  await createSession(page, appBase);
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
  // AGENTS.md is inlined verbatim; the workspace skill contributes a listing line.
  expect(await mock.capturedContains("E2E_AGENTS_MARKER")).toBe(true);
  expect(await mock.capturedContains("- e2e-skill: E2E_SKILL_DESC")).toBe(true);
});

test("F2: shared plugin-library skill loads into the system prompt", async ({
  page,
  appBase,
  mock,
}) => {
  await runTurnAndCapture(page, appBase, mock);
  expect(await mock.capturedContains("# Shared skills")).toBe(true);
  expect(await mock.capturedContains("- e2e-shared-skill: E2E_SHARED_DESC")).toBe(true);
});

test("F3: SessionStart hook output loads as the session bootstrap", async ({
  page,
  appBase,
  mock,
}) => {
  await runTurnAndCapture(page, appBase, mock);
  expect(await mock.capturedContains("# Session bootstrap")).toBe(true);
  expect(await mock.capturedContains("E2E_BOOTSTRAP_MARKER")).toBe(true);
});
