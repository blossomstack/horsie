// Reusable UI actions built on the data-testid hooks. Keep flow logic here so
// the specs read as intent, not selector plumbing.

import { expect, type Page } from "@playwright/test";

/**
 * Start a new-chat draft: navigate to `/`, wait for the draft config bar, and
 * optionally pick a model. Creates NOTHING server-side — the session is created
 * by the first `sendMessage`.
 */
export async function createSession(
  page: Page,
  appBase: string,
  opts: { model?: string } = {},
): Promise<void> {
  await page.goto(appBase);
  await expect(page.getByTestId("config-model")).toBeVisible();
  if (opts.model) {
    await page.getByTestId("config-model").click();
    await page
      .locator(`[data-testid="model-option"][data-value="${opts.model}"]`)
      .click();
  }
}

/**
 * Type a message and send it (Enter). On a draft (`/`) this creates the session
 * and waits for the `/sessions/:id` route. Returns the session id.
 */
export async function sendMessage(page: Page, text: string): Promise<string> {
  const onDraft = new URL(page.url()).pathname === "/";
  const input = page.getByTestId("composer-input");
  await input.fill(text);
  await input.press("Enter");
  if (onDraft) await page.waitForURL(/\/sessions\/[0-9a-f-]+$/);
  const id = new URL(page.url()).pathname.split("/").pop();
  if (!id) throw new Error("no session id in URL after send");
  return id;
}

/** Assert the session status badge shows the given SessionStatusKind value. */
export async function expectStatus(page: Page, status: string): Promise<void> {
  await expect(page.getByTestId("status-badge")).toHaveAttribute("data-status", status);
}
