// Group U — the one box: paste a catalogue URL, get a source rather than a
// failed install, pick a plugin from it, and see it in the library.
//
// This is the flow the whole design exists to produce, and the only test that
// covers all three sections of the Skills page together. It runs against a real
// `file://` git marketplace, so it exercises clone → parse index → resolve entry
// → pack → persist — the path that was broken for every marketplace-shaped repo
// before this change, including the one that motivated plugin support at all.

import { test, expect } from "./fixtures";

test("U1: a catalogue URL registers a source, and an entry installs from it", async ({
  page,
  appBase,
  marketplaceUrl,
}) => {
  await page.goto(`${appBase}/settings/skills`);

  await page.getByLabel("Git URL").fill(marketplaceUrl);
  await page.getByRole("button", { name: "Install" }).click();

  // Not an install, and not an error: a source, already open.
  const row = page
    .getByTestId("marketplace-row")
    .filter({ hasText: "e2e-market" });
  await expect(row).toBeVisible();
  await expect(row).toContainText("2 plugins");
  await expect(page.getByTestId("entry-install-e2e-alpha")).toBeVisible();

  await page.getByTestId("entry-install-e2e-beta").click();

  // The bundle lands in the library carrying where it came from…
  const bundle = page.getByTestId("bundle-row").filter({ hasText: "e2e-beta" });
  await expect(bundle).toBeVisible();
  await expect(bundle).toContainText("e2e-market");
  // …and the catalogue stops offering it.
  await expect(page.getByTestId("entry-install-e2e-beta")).toBeDisabled();
});

test("U2: an installed bundle lists what it offers, and `/` completes it", async ({
  page,
  appBase,
}) => {
  // Depends on U1 having installed `e2e-beta`. The suite is serial and these
  // live in one file so that ordering is a fact rather than a hope.
  await page.goto(`${appBase}/settings/skills`);
  const bundle = page.getByTestId("bundle-row").filter({ hasText: "e2e-beta" });
  await expect(bundle).toBeVisible();

  // The catalogue derived at ingest, counted by kind…
  const disclosure = bundle.getByRole("button", { expanded: false });
  await expect(disclosure).toContainText("1 skill");
  await disclosure.click();
  // …and listed as the exact string a user types.
  await expect(bundle).toContainText("/e2e-beta-skill");

  // The same catalogue, reached from the composer with no session in existence.
  await page.goto(`${appBase}/`);
  await page.getByTestId("new-session-button").click();
  await page.getByTestId("config-skills").click();
  await page
    .locator("label")
    .filter({ hasText: "e2e-beta" })
    .getByRole("checkbox")
    .check();
  await page.keyboard.press("Escape");

  await page.getByTestId("composer-input").fill("/");
  await expect(page.getByTestId("entry-menu")).toBeVisible();
  await expect(page.getByTestId("entry-menu")).toContainText("/e2e-beta-skill");
  // A bare `/` also offers what horsie answers itself, whatever bundles are
  // installed.
  await expect(page.getByTestId("entry-menu")).toContainText("/compact");

  // Narrowed to one entry before pressing Enter: with builtins listed first, a
  // bare `/` would pick `/compact`, and this test is about Enter *picking*
  // rather than about which entry happens to lead the menu.
  await page.getByTestId("composer-input").fill("/e2e");
  await expect(page.getByTestId("entry-menu")).toContainText("/e2e-beta-skill");

  // Enter picks rather than sends — the hazard the whole key ordering exists
  // to prevent.
  await page.getByTestId("composer-input").press("Enter");
  await expect(page.getByTestId("composer-input")).toHaveValue(
    "/e2e-beta-skill ",
  );
  await expect(page.getByTestId("entry-menu")).toHaveCount(0);
});
