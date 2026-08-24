// Authoring, from the settings page: a plugin created here shows up in the
// bundle library beside the cloned ones, and says which of the two it is.

import { test, expect } from "./fixtures";

test.describe("authored plugins", () => {
  test("a plugin created here joins the library as an authored bundle", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/skills`);
    const section = page.getByTestId("authored-section");
    await expect(section).toBeVisible();
    await expect(section.getByText(/Nothing authored yet/)).toBeVisible();

    await page.getByTestId("authored-plugin-name").fill("field-notes");
    await page.getByRole("button", { name: "Create" }).click();

    const row = page.getByTestId("authored-plugin-row");
    await expect(row).toBeVisible();
    await expect(row.getByText("field-notes")).toBeVisible();
    // The generation, which is what a runtime fetches by — it moves on every
    // edit, so it starts at 1 and is worth showing.
    await expect(row.getByText("gen 1")).toBeVisible();

    // It publishes empty. The plugin is what an agent is assigned, and the
    // skills are written into it afterwards — so it has to be pickable during
    // the period it is still being filled in.
    const bundles = page.getByTestId("bundle-row");
    await expect(bundles).toHaveCount(2);
    const authored = bundles.filter({ hasText: "field-notes" });
    await expect(authored.getByText("authored here")).toBeVisible();
    // Nothing to re-read and nothing to uninstall: it is deleted where it is
    // written, in the section above.
    await expect(authored.getByText("Update")).toHaveCount(0);
    await expect(authored.getByLabel("Delete bundle")).toHaveCount(0);

    // And an agent can select it, which is the whole point of publishing it
    // before it holds anything.
    await page.goto(`${appBase}/`);
    await page.getByTestId("new-session-button").click();
    await page.getByTestId("config-skills").click();
    await expect(
      page.locator("label").filter({ hasText: "field-notes" }),
    ).toBeVisible();
  });

  test("the e2e fixture bundle reports which packaging it uses", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/skills`);
    const bundle = page.getByTestId("bundle-row").first();
    await expect(bundle.getByText("e2e-plugin")).toBeVisible();
    await expect(bundle.getByText("claude")).toBeVisible();
  });
});
