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

  test("R4: text size scales the whole interface and survives a reload", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/appearance`);

    // Default carries no attribute, same convention as Console.
    await expect(page.locator("html")).not.toHaveAttribute("data-text-size", /.+/);
    const rootSize = () =>
      page.evaluate(() =>
        parseFloat(getComputedStyle(document.documentElement).fontSize),
      );
    const title = page.getByRole("heading", { level: 1, name: "Appearance" });
    const base = await rootSize();
    const baseTitle = await title.evaluate(
      (el) => el.getBoundingClientRect().height,
    );

    await page.getByTestId("text-size-option-large").click();
    await expect(page.locator("html")).toHaveAttribute("data-text-size", "large");
    expect(await rootSize()).toBeGreaterThan(base);
    // Not just the type: every rem in the build rides the same root, so the
    // slots grow with what goes in them.
    expect(
      await title.evaluate((el) => el.getBoundingClientRect().height),
    ).toBeGreaterThan(baseTitle);

    await page.getByTestId("text-size-option-compact").click();
    expect(await rootSize()).toBeLessThan(base);

    // Resolved before first paint, like theme and skin — a text size settled
    // after hydration would reflow the entire layout in front of the user.
    await page.goto(`${appBase}/settings/appearance`, {
      waitUntil: "domcontentloaded",
    });
    await expect(page.locator("html")).toHaveAttribute("data-text-size", "compact");

    await page.getByTestId("text-size-option-default").click();
    await expect(page.locator("html")).not.toHaveAttribute("data-text-size", /.+/);
  });
});
