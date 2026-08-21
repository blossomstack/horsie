// Settings/admin navigation: the nav column, deep links, legacy redirects, and
// the unsaved-changes guard on the one page that still batches its save.

import { test, expect } from "./fixtures";

test.describe("settings navigation", () => {
  test("index redirects to Models and the nav lists every page", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings`);
    await expect(page).toHaveURL(/\/settings\/models$/);

    const nav = page.getByTestId("settings-nav");
    for (const label of [
      "Models",
      "Runtimes",
      "Skills",
      "Memory",
      "Integrations",
      "Appearance",
      "Account",
    ]) {
      await expect(nav.getByRole("link", { name: label })).toBeVisible();
    }

    await nav.getByTestId("settings-nav-runtimes").click();
    await expect(page).toHaveURL(/\/settings\/runtimes$/);
    await expect(
      page.getByRole("heading", { name: "Runtimes", level: 1 }),
    ).toBeVisible();

    await nav.getByTestId("settings-nav-memory").click();
    await expect(page).toHaveURL(/\/settings\/memory$/);
    await expect(
      page.getByRole("heading", { name: "Memory", level: 1 }),
    ).toBeVisible();
  });

  test("legacy top-level paths redirect into settings", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/skills`);
    await expect(page).toHaveURL(/\/settings\/skills$/);

    await page.goto(`${appBase}/memory`);
    await expect(page).toHaveURL(/\/settings\/memory$/);

    await page.goto(`${appBase}/admin`);
    await expect(page).toHaveURL(/\/admin\/model-cards$/);
  });

  // The guard now protects exactly one page. Settings save per item, so there
  // is nothing there to lose on navigation; the GitHub App credentials form is
  // the last batched form in the product, and the worst one to lose input on.
  test("leaving the credentials form with unsaved edits prompts first", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/admin/github-app`);
    await page.getByLabel("Client ID").fill("Iv1.abc123");
    await expect(page.getByTestId("settings-save")).toBeEnabled();

    // Dismiss the prompt: stay put, edit intact.
    page.once("dialog", (d) => d.dismiss());
    await page.getByTestId("settings-nav-model-cards").click();
    await expect(page).toHaveURL(/\/admin\/github-app$/);

    // Accept it: navigate away and drop the edit.
    page.once("dialog", (d) => d.accept());
    await page.getByTestId("settings-nav-model-cards").click();
    await expect(page).toHaveURL(/\/admin\/model-cards$/);
  });

  // An unmatched route used to render a blank white page: zero DOM, not even
  // the rail, so the only escape was the URL bar. `/admin/github` is not a
  // hypothetical — it is one character from the real `/admin/github-app`.
  test("an unmatched route renders a not-found page with navigation intact", async ({
    page,
    appBase,
  }) => {
    for (const path of ["/admin/github", "/nonsense", "/settings/memory/17"]) {
      await page.goto(`${appBase}${path}`);
      const notFound = page.getByTestId("not-found-page");
      await expect(notFound).toBeVisible();
      // The path is named, so the typo is visible rather than guessed at.
      await expect(notFound).toContainText(path);
      // The actual defect was the missing navigation, not the missing copy:
      // the blank page had no rail at all, so nothing else was reachable.
      await expect(page.getByTestId("agents-link")).toBeVisible();
      await expect(page.getByTestId("new-session-button")).toBeVisible();
    }

    // And it is a dead end no longer.
    await page.getByRole("link", { name: "your sessions" }).click();
    // No trailing slash: under the project basename, react-router renders
    // `to="/"` as the basename itself.
    await expect(page).toHaveURL(appBase);
  });

  // `callback_base` is the only override for a wrong OAuth `redirect_uri`, and
  // the form did not submit it — so every Admin save silently wiped it and
  // re-broke the flow. Setting it by API worked; one unrelated edit here undid
  // that.
  test("the GitHub App form round-trips the callback base", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/admin/github-app`);
    await page.getByLabel("Callback base URL").fill("https://horsie.example.com");
    await page.getByLabel("Client ID").fill("Iv1.round.trip");
    await page.getByTestId("settings-save").click();

    await page.reload();
    await expect(page.getByLabel("Callback base URL")).toHaveValue(
      "https://horsie.example.com",
    );

    // And an edit that has nothing to do with it leaves it alone — this is the
    // exact sequence that used to destroy it.
    await page.getByLabel("Client ID").fill("Iv1.something.else");
    await page.getByTestId("settings-save").click();
    await page.reload();
    await expect(page.getByLabel("Callback base URL")).toHaveValue(
      "https://horsie.example.com",
    );
  });
});
