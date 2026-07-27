// Settings/admin navigation: the nav column, deep links, legacy redirects, and
// the unsaved-changes guard on the batched-save pages.

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

  test("leaving a page with unsaved edits prompts first", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/models`);
    await page.getByRole("button", { name: "Add model" }).click();
    await expect(page.getByTestId("settings-save")).toBeEnabled();

    // Dismiss the prompt: stay put, edit intact.
    page.once("dialog", (d) => d.dismiss());
    await page.getByTestId("settings-nav-runtimes").click();
    await expect(page).toHaveURL(/\/settings\/models$/);

    // Accept it: navigate away and drop the edit.
    page.once("dialog", (d) => d.accept());
    await page.getByTestId("settings-nav-runtimes").click();
    await expect(page).toHaveURL(/\/settings\/runtimes$/);
  });
});
