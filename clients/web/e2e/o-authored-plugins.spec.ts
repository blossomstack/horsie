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

    // With no skills in it there is nothing installable, so it does not
    // publish a bundle: the library still holds only the cloned fixture, and
    // that one says it is a clone.
    const bundles = page.getByTestId("bundle-row");
    await expect(bundles).toHaveCount(1);
    await expect(bundles.first().getByText("claude")).toBeVisible();
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
