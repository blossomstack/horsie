// Model cards: admin-page CRUD over the seeded catalog, and the Settings
// model form's id autocomplete + limit prefill.

import { test, expect } from "./fixtures";

test.describe("model cards", () => {
  test("admin page lists seeded cards and supports CRUD", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/admin/model-cards`);

    // The catalog reads as a list now: each row states the card, and the
    // editor opens on request rather than every card rendering as a form.
    const seeded = page.getByTestId("model-card-row-claude-sonnet-4-6");
    await expect(seeded).toBeVisible();
    await expect(seeded).toContainText("claude-sonnet-4-6");

    // The info key reveals the values the row has no room for.
    await seeded.getByTestId("model-card-info-claude-sonnet-4-6").click();
    await expect(seeded).toContainText("Thinking dialect");

    // Create.
    await page.getByTestId("add-model-card").click();
    const draft = page.getByTestId("model-card-editor-new");
    await draft.getByLabel("Model id").fill("e2e-model-1");
    await draft.getByLabel("Name").fill("E2E Model");
    await draft.getByLabel("Context window (optional)").fill("123456");
    await draft.getByLabel("Max tokens (optional)").fill("4096");
    await draft.getByTestId("model-card-save").click();

    const row = page.getByTestId("model-card-row-e2e-model-1");
    await expect(row).toBeVisible();
    await expect(row).toContainText("E2E Model");

    // Edit the name through the row's own edit key. model_id is the id of
    // record, so it is fixed once saved.
    await row.getByTestId("model-card-edit-e2e-model-1").click();
    const editor = page.getByTestId("model-card-editor-e2e-model-1");
    await expect(editor.getByLabel("Model id")).toBeDisabled();
    await editor.getByLabel("Name").fill("E2E Model Renamed");
    await editor.getByTestId("model-card-save").click();
    await expect(
      page.getByTestId("model-card-row-e2e-model-1"),
    ).toContainText("E2E Model Renamed");

    // Persists across reload.
    await page.reload();
    await expect(page.getByTestId("model-card-row-e2e-model-1")).toBeVisible();

    // Delete (accept the confirm dialog).
    page.on("dialog", (d) => d.accept());
    await page.getByTestId("model-card-delete-e2e-model-1").click();
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
    // Corrected in migration 0012: Sonnet 4.6 is 1M context / 128K output, not
    // the 200K / 16K the original bundled catalog shipped.
    await expect(
      page.getByLabel("Context window (optional)").last(),
    ).toHaveValue("1000000");
    await expect(page.getByLabel("Max tokens (optional)").last()).toHaveValue(
      "128000",
    );
  });

  test("a DeepSeek card prefills the forced-tools flag but leaves providers alone", async ({
    page,
    appBase,
  }) => {
    // deepseek-v4-flash is seeded with forcedToolsDisableThinking and a
    // baseUrl. Picking it must copy the flag onto the model draft and leave
    // every provider's base URL untouched — prefilling that is out of scope.
    await page.goto(`${appBase}/settings/models`);

    // Providers are collapsed rows now, and each states its endpoint on the
    // row itself — so the invariant is checked without opening an editor that
    // would only be another chance to change the value under test.
    const providerRow = page.getByTestId(/^provider-row-/).first();
    const before = await providerRow.textContent();

    await page.getByRole("button", { name: "Add model" }).click();
    const idInput = page.getByTestId("model-id-input").last();
    await idInput.fill("deepseek-v4-flash");

    const suggestion = page.getByTestId(
      "model-card-suggestion-deepseek-v4-flash",
    );
    await expect(suggestion).toBeVisible();
    await suggestion.dispatchEvent("mousedown");

    await expect(idInput).toHaveValue("deepseek-v4-flash");
    await expect(page.getByTestId("model-forced-tools").last()).toBeChecked();
    await expect(
      page.getByLabel("Context window (optional)").last(),
    ).toHaveValue("1048576");
    expect(await providerRow.textContent()).toBe(before);
  });
});
