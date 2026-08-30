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

    // Delete (accept the in-app confirm).
    await page.getByTestId("model-card-delete-e2e-model-1").click();
    await page.getByTestId("confirm-accept").click();
    await expect(page.getByTestId("model-card-row-e2e-model-1")).toHaveCount(0);
  });

  // The save is a full replacement, so any field this editor cannot show is a
  // field every save silently clears — and `seed_if_missing` never repairs an
  // existing row, so the loss is permanent from inside the product. That has
  // already happened once here, to the thinking config. This pins the vision
  // flags against a repeat.
  test("editing an unrelated field does not clear a card's vision flags", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/admin/model-cards`);

    const row = page.getByTestId("model-card-row-claude-sonnet-4-6");
    await row.getByTestId("model-card-edit-claude-sonnet-4-6").click();
    const editor = page.getByTestId("model-card-editor-claude-sonnet-4-6");

    // Seeded as a model that can be shown both.
    await expect(editor.getByTestId("model-card-supports-images")).toBeChecked();
    await expect(
      editor.getByTestId("model-card-supports-documents"),
    ).toBeChecked();

    // Touch something else entirely, and save.
    await editor.getByLabel("Name").fill("Claude Sonnet 4.6 (edited)");
    await editor.getByTestId("model-card-save").click();
    await expect(row).toContainText("Claude Sonnet 4.6 (edited)");

    // Reload so this reads the stored row rather than local state.
    await page.reload();
    await page
      .getByTestId("model-card-row-claude-sonnet-4-6")
      .getByTestId("model-card-edit-claude-sonnet-4-6")
      .click();
    const reopened = page.getByTestId("model-card-editor-claude-sonnet-4-6");
    await expect(reopened.getByTestId("model-card-supports-images")).toBeChecked();
    await expect(
      reopened.getByTestId("model-card-supports-documents"),
    ).toBeChecked();
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
  // Editing a card used to wipe `thinkingEfforts`, `defaultThinkingEffort` and
  // `thinkingDialect`: a full-replacement PUT plus a form that could not
  // display those three fields. `seed_if_missing` never repairs an existing
  // row, so one operator bumping a max-token count destroyed a model's
  // thinking config permanently, with no way to restore it in the product.
  test("editing an unrelated field keeps the card's thinking config", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/admin/model-cards`);

    const row = page.getByTestId("model-card-row-claude-sonnet-4-6");
    await row.getByTestId("model-card-info-claude-sonnet-4-6").click();
    await expect(row).toContainText("anthropic_effort");

    // Change one thing that has nothing to do with thinking — the exact
    // sequence that used to destroy it.
    await row.getByTestId("model-card-edit-claude-sonnet-4-6").click();
    const editor = page.getByTestId("model-card-editor-claude-sonnet-4-6");
    // The editor can now see them at all, which is the other half of the fix.
    await expect(editor.getByTestId("model-card-dialect")).toHaveValue(
      "anthropic_effort",
    );
    await editor.getByLabel("Max tokens (optional)").fill("32768");
    await editor.getByTestId("model-card-save").click();

    await page.reload();
    const after = page.getByTestId("model-card-row-claude-sonnet-4-6");
    await after.getByTestId("model-card-info-claude-sonnet-4-6").click();
    await expect(after).toContainText("anthropic_effort");
    // And the efforts survived too, not just the dialect.
    await expect(after).not.toContainText("Thinking efforts —");
  });
});
