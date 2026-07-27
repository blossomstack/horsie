// Model cards: admin-page CRUD over the seeded catalog, and the Settings
// model form's id autocomplete + limit prefill.

import { test, expect } from "./fixtures";

test.describe("model cards", () => {
  test("admin page lists seeded cards and supports CRUD", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/admin/model-cards`);

    // Bundled seed is present (the real server seeds at startup). The row
    // renders the id as an input value, not text, so assert on the row.
    const seeded = page.getByTestId("model-card-row-claude-sonnet-4-6");
    await expect(seeded).toBeVisible();
    await expect(seeded.getByLabel("Model id")).toHaveValue(
      "claude-sonnet-4-6",
    );

    // Create.
    await page.getByTestId("add-model-card").click();
    const draft = page.getByTestId("model-card-row-new");
    await draft.getByLabel("Model id").fill("e2e-model-1");
    await draft.getByLabel("Name").fill("E2E Model");
    await draft.getByLabel("Context window (optional)").fill("123456");
    await draft.getByLabel("Max tokens (optional)").fill("4096");
    await draft.getByTestId("model-card-save").click();

    const row = page.getByTestId("model-card-row-e2e-model-1");
    await expect(row).toBeVisible();
    // model_id is immutable once saved.
    await expect(row.getByLabel("Model id")).toBeDisabled();

    // Edit the name.
    await row.getByLabel("Name").fill("E2E Model Renamed");
    await row.getByTestId("model-card-save").click();
    await expect(row.getByLabel("Name")).toHaveValue("E2E Model Renamed");

    // Persists across reload.
    await page.reload();
    await expect(page.getByTestId("model-card-row-e2e-model-1")).toBeVisible();

    // Delete (accept the confirm dialog).
    page.on("dialog", (d) => d.accept());
    await page
      .getByTestId("model-card-row-e2e-model-1")
      .getByTestId("model-card-remove")
      .click();
    await expect(page.getByTestId("model-card-row-e2e-model-1")).toHaveCount(0);
  });

  test("settings model form autocompletes model id and prefills limits", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/models`);
    await page.getByRole("button", { name: "Add model" }).click();

    // The new row is appended last (global-setup already seeded one model).
    const idInput = page.getByTestId("model-id-input").last();
    await idInput.fill("claude-sonnet");

    const suggestion = page.getByTestId(
      "model-card-suggestion-claude-sonnet-4-6",
    );
    await expect(suggestion).toBeVisible();
    await suggestion.dispatchEvent("mousedown");

    await expect(idInput).toHaveValue("claude-sonnet-4-6");
    await expect(
      page.getByLabel("Context window (optional)").last(),
    ).toHaveValue("200000");
    await expect(page.getByLabel("Max tokens (optional)").last()).toHaveValue(
      "16384",
    );
  });
});
