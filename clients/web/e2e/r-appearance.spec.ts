// Group R — the theme system: four worlds over the same layouts, a light/dark
// choice that can defer to the OS, and all of it resolved before first paint
// so nothing flashes the wrong world and then corrects itself.

import { test, expect } from "./fixtures";

test.describe("appearance", () => {
  test("R1: choosing a theme applies it and it survives a reload", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/appearance`);

    // Paper is the default and deliberately carries no attribute, so its
    // selectors keep the specificity index.css was written against.
    await expect(page.locator("html")).not.toHaveAttribute("data-skin", /.+/);

    await page.getByTestId("skin-option-signal").click();
    await expect(page.locator("html")).toHaveAttribute("data-skin", "signal");

    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("data-skin", "signal");

    // And back: choosing Paper removes the attribute rather than setting it
    // to a value no CSS block matches.
    await page.getByTestId("skin-option-paper").click();
    await expect(page.locator("html")).not.toHaveAttribute("data-skin", /.+/);
  });

  test("R2: the theme is resolved before first paint", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/appearance`);
    await page.getByTestId("skin-option-signal").click();
    await expect(page.locator("html")).toHaveAttribute("data-skin", "signal");

    // The inline script in index.html sets the attributes, so the very first
    // document the browser paints is already the chosen world. Reading the
    // attribute at `domcontentloaded` — before React has mounted — is what
    // distinguishes that from a post-hydration correction the user would see
    // as a flash of Paper.
    await page.goto(`${appBase}/settings/appearance`, {
      waitUntil: "domcontentloaded",
    });
    await expect(page.locator("html")).toHaveAttribute("data-skin", "signal");

    await page.getByTestId("skin-option-paper").click();
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

    // The shipped density carries no attribute.
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

    // Resolved before first paint, like the world and the exposure — a size settled
    // after hydration would reflow the entire layout in front of the user.
    await page.goto(`${appBase}/settings/appearance`, {
      waitUntil: "domcontentloaded",
    });
    await expect(page.locator("html")).toHaveAttribute("data-text-size", "compact");

    await page.getByTestId("text-size-option-default").click();
    await expect(page.locator("html")).not.toHaveAttribute("data-text-size", /.+/);
  });
});

test.describe("language", () => {
  test("R5: choosing a language translates the app and survives a reload", async ({
    page,
    appBase,
  }) => {
    // Start on the new-session view, which draws the config row: its legends
    // come from `configPickers`, which reads the catalogue through the global
    // `t` rather than through `useTranslation`. Nothing in that subtree
    // subscribes to the language, so it is the surface a partial switch
    // leaves in English — which is what the remount in `App` exists for.
    await page.goto(appBase);
    await expect(page.getByTestId("config-tools")).toHaveAttribute(
      "aria-label",
      /Tools/,
    );

    await page.goto(`${appBase}/settings/appearance`);
    await expect(page.locator("html")).toHaveAttribute("lang", "en");

    await page.getByTestId("locale-option-zh-Hans").click();
    await expect(page.locator("html")).toHaveAttribute("lang", "zh-Hans");
    // The page's own heading, and the rail beside it: a switch that only
    // repaints the settings page is the failure this asserts against, since
    // most of the app's strings arrive through helpers rather than through a
    // component that subscribed to the language.
    await expect(
      page.getByRole("heading", { level: 1, name: "外观" }),
    ).toBeVisible();
    await expect(page.getByTestId("agents-link")).toContainText("智能体");

    // The unsubscribed subtree moved too, without a reload.
    await page.goto(appBase);
    await expect(page.getByTestId("config-tools")).toHaveAttribute(
      "aria-label",
      /工具/,
    );

    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("lang", "zh-Hans");
    await page.goto(`${appBase}/settings/appearance`);
    await expect(page.getByTestId("agents-link")).toContainText("智能体");

    // Traditional is a separate catalogue, not a character conversion of the
    // Simplified one — 智慧代理 rather than 智能體 is the tell.
    await page.getByTestId("locale-option-zh-Hant").click();
    await expect(page.locator("html")).toHaveAttribute("lang", "zh-Hant");
    await expect(page.getByTestId("agents-link")).toContainText("智慧代理");

    await page.getByTestId("locale-option-en").click();
    await expect(page.locator("html")).toHaveAttribute("lang", "en");
    await expect(page.getByTestId("agents-link")).toContainText("Agents");
  });

  test("R6: System follows the browser's language", async ({ browser, appBase }) => {
    // A fresh context, because the choice is per-browser and `system` is only
    // the default until something has been picked.
    const context = await browser.newContext({ locale: "zh-TW" });
    const page = await context.newPage();
    await page.goto(`${appBase}/settings/appearance`);

    await expect(page.locator("html")).toHaveAttribute("lang", "zh-Hant");
    await expect(page.getByTestId("agents-link")).toContainText("智慧代理");
    await context.close();
  });
});
