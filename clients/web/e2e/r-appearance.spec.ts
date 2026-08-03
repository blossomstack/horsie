// Group R — the theme system: four skins over the same layouts, a light/dark
// choice that can defer to the OS, and both surviving a reload without the
// wrong world flashing first.

import { test, expect } from "./fixtures";

test.describe("appearance", () => {
  test("R1: choosing a skin applies it and it survives a reload", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/appearance`);

    // Console is the default and deliberately carries no attribute, so its
    // selectors keep the specificity index.css was written against.
    await expect(page.locator("html")).not.toHaveAttribute("data-skin", /.+/);

    await page.getByTestId("skin-option-paper").click();
    await expect(page.locator("html")).toHaveAttribute("data-skin", "paper");

    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("data-skin", "paper");

    // And back: choosing Console removes the attribute rather than setting it
    // to a value no CSS block matches.
    await page.getByTestId("skin-option-console").click();
    await expect(page.locator("html")).not.toHaveAttribute("data-skin", /.+/);
  });

  test("R2: the skin is resolved before first paint", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/appearance`);
    await page.getByTestId("skin-option-slate").click();
    await expect(page.locator("html")).toHaveAttribute("data-skin", "slate");

    // The inline script in index.html sets both attributes, so the very first
    // document the browser paints is already the chosen world. Reading the
    // attribute at `domcontentloaded` — before React has mounted — is what
    // distinguishes that from a post-hydration correction the user would see
    // as a flash of Console.
    await page.goto(`${appBase}/settings/appearance`, {
      waitUntil: "domcontentloaded",
    });
    await expect(page.locator("html")).toHaveAttribute("data-skin", "slate");
  });

  test("R3: light/dark is a three-way choice and System follows the OS", async ({
    page,
    appBase,
  }) => {
    await page.emulateMedia({ colorScheme: "light" });
    await page.goto(`${appBase}/settings/appearance`);

    await page.getByTestId("mode-option-system").click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");

    // Still on System, the OS flipping flips the app — the old two-state
    // toggle sampled the preference once and then ignored it.
    await page.emulateMedia({ colorScheme: "dark" });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");

    // An explicit choice stops following.
    await page.getByTestId("mode-option-light").click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    await page.emulateMedia({ colorScheme: "dark" });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  });
});
