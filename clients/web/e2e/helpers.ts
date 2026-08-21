// Reusable UI actions built on the data-testid hooks. Keep flow logic here so
// the specs read as intent, not selector plumbing.

import { readRuntimeInfo } from "./harness";
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
/**
 * The path every route in this suite is relative to: `/p/<project>`.
 *
 * Every page horsie serves lives under a project, and the web router's basename
 * is that prefix — so an assertion on a bare `/agents` is asserting about a
 * document the app never renders.
 */
export function projectRoot(): string {
  return `/p/${readRuntimeInfo().project}`;
}

/**
 * Sign in if asked, then report the project `/` sent the browser to.
 *
 * For the two suites that run their own auth-enabled server. `defaultProject`
 * cannot serve them: it fetches with no credential, which such a server answers
 * with a 401 — and the redirect at `/` is behind the login form for the same
 * reason. So this does what a person does: open the root, sign in, and read
 * where it landed.
 */
export async function projectOf(
  page: Page,
  baseURL: string,
  password: string,
): Promise<string> {
  await page.goto(baseURL);
  const form = page.getByTestId("login-form");
  if (await form.isVisible().catch(() => false)) {
    await page.getByTestId("login-password").fill(password);
    await page.getByTestId("login-submit").click();
    await expect(form).toHaveCount(0);
  }
  await page.waitForURL(/\/p\/[^/]+/);
  const id = new URL(page.url()).pathname.split("/")[2];
  if (!id) throw new Error(`no project in ${page.url()}`);
  return id;
}

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
  const onDraft = new URL(page.url()).pathname === projectRoot();
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
