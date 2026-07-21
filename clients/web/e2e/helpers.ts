// Reusable UI actions built on the data-testid hooks. Keep flow logic here so
// the specs read as intent, not selector plumbing.

import { expect, type Page } from "@playwright/test";

/**
 * Open the New Session modal, create a session (default model + the single
 * active `e2e` vendor), wait for the transcript route, and return the id.
 */
export async function createSession(
  page: Page,
  appBase: string,
  opts: { name?: string; model?: string } = {},
): Promise<string> {
  await page.goto(appBase);
  await page.getByTestId("new-session-button").click();
  await expect(page.getByTestId("model-select")).toBeVisible();
  if (opts.name) await page.getByTestId("session-name-input").fill(opts.name);
  // Option `value` is the model alias; the default is the first-listed model.
  if (opts.model) await page.getByTestId("model-select").selectOption(opts.model);
  // Auto-waits for the button to be enabled (models loaded, not submitting).
  await page.getByTestId("create-session-submit").click();
  await page.waitForURL(/\/sessions\/[0-9a-f-]+$/);
  const id = new URL(page.url()).pathname.split("/").pop();
  if (!id) throw new Error("no session id in URL after create");
  return id;
}

/** Type a message into the composer and send it (Enter). */
export async function sendMessage(page: Page, text: string): Promise<void> {
  const input = page.getByTestId("composer-input");
  await input.fill(text);
  await input.press("Enter");
}

/** Assert the session status badge shows the given SessionStatusKind value. */
export async function expectStatus(page: Page, status: string): Promise<void> {
  await expect(page.getByTestId("status-badge")).toHaveAttribute("data-status", status);
}
