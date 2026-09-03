import { expect, test } from "./fixtures";

test("B1: the desktop sidebar can be hidden and restored", async ({
  page,
  appBase,
}, testInfo) => {
  await page.goto(`${appBase}/`);
  await expect(page.getByTestId("hide-sidebar-button")).toBeVisible();

  const expanded = testInfo.outputPath("sidebar-expanded.png");
  await page.screenshot({ path: expanded, fullPage: true });
  await testInfo.attach("sidebar-expanded", {
    path: expanded,
    contentType: "image/png",
  });

  await page.getByTestId("hide-sidebar-button").click();
  await expect(page.getByTestId("show-sidebar-button")).toBeVisible();
  await expect(page.getByTestId("hide-sidebar-button")).not.toBeVisible();
  for (const destination of [
    "inbox",
    "agents",
    "environments",
    "routines",
    "workflows",
    "settings",
  ]) {
    await expect(page.getByTestId(`collapsed-${destination}-link`)).toBeVisible();
  }

  await page.waitForTimeout(300);
  const collapsed = testInfo.outputPath("sidebar-collapsed.png");
  await page.screenshot({ path: collapsed, fullPage: true });
  await testInfo.attach("sidebar-collapsed", {
    path: collapsed,
    contentType: "image/png",
  });

  await page.getByTestId("show-sidebar-button").hover();
  const collapsedHover = testInfo.outputPath("sidebar-collapsed-hover.png");
  await page.screenshot({ path: collapsedHover, fullPage: true });
  await testInfo.attach("sidebar-collapsed-hover", {
    path: collapsedHover,
    contentType: "image/png",
  });

  await page.reload();
  await expect(page.getByTestId("show-sidebar-button")).toBeVisible();

  await page.getByTestId("show-sidebar-button").click();
  await expect(page.getByTestId("hide-sidebar-button")).toBeVisible();
});
