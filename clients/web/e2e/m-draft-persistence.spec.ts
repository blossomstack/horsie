// Group M — the new-session draft persists to localStorage and is restored
// after a reload. The e2e vendor is non-provisioning, so only the runtime and
// model chips are exercised here; selection-set restore is unit-tested.
import { expect, test } from "./fixtures";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("M1: model selection survives a page reload", async ({ page, appBase }) => {
  await page.goto(appBase);
  await expect(page.getByTestId("config-model")).toBeVisible();

  // Default is the first model; switch to the other seeded one.
  await expect(page.getByTestId("config-model")).toContainText("mock-sonnet");
  await page.getByTestId("config-model").click();
  await page.locator('[data-testid="model-option"][data-value="openai-mock"]').click();
  await expect(page.getByTestId("config-model")).toContainText("openai-mock");

  await page.reload();
  await expect(page.getByTestId("config-model")).toContainText("openai-mock");
});

test("M2: clearing the stored draft restores server defaults", async ({ page, appBase }) => {
  await page.goto(appBase);
  await page.getByTestId("config-model").click();
  await page.locator('[data-testid="model-option"][data-value="openai-mock"]').click();
  await expect(page.getByTestId("config-model")).toContainText("openai-mock");

  await page.evaluate(() => localStorage.removeItem("horsie-session-draft"));
  await page.reload();
  await expect(page.getByTestId("config-model")).toContainText("mock-sonnet");
});
