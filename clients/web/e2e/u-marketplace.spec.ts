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
