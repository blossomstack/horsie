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
});
