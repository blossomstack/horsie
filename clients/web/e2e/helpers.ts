// Reusable UI actions built on the data-testid hooks. Keep flow logic here so
// the specs read as intent, not selector plumbing.

import { expect, type Page } from "@playwright/test";

/**
 * Start a new-chat draft: navigate to `/`, wait for the draft config bar, and
 * optionally pick a model and skill bundles. Creates NOTHING server-side — the
 * session is created by the first `sendMessage`.
 *
 * `skills` is opt-in per spec rather than a default on the bundle, because a
 * selected bundle is fetched and unpacked by the runtime before the session can
 * take a turn. Paying that on all ~75 specs slows every one of them down and
 * makes the composer's send-while-starting race reachable; only the specs that
 * assert on plugin content should ask for it.
 */
export async function createSession(
  page: Page,
  appBase: string,
  opts: { model?: string; skills?: string[] } = {},
): Promise<void> {
  await page.goto(appBase);
  await expect(page.getByTestId("config-model")).toBeVisible();
  if (opts.model) {
    await page.getByTestId("config-model").click();
    await page
      .locator(`[data-testid="model-option"][data-value="${opts.model}"]`)
      .click();
  }
  if (opts.skills?.length) {
    const picker = page.getByTestId("config-skills");
    await picker.click();
    for (const name of opts.skills) {
      await page
        .locator("label", { hasText: new RegExp(`^${name}$`) })
        .getByRole("checkbox")
        .check();
    }
    // Close the popover so the composer is clickable again.
    await picker.click();
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
