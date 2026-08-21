// Group Z — projects: the scope every other page belongs to.
//
// The server-side isolation has its own suite (`project_isolation_http.rs`).
// What is only testable here is that the *browser* honours it: that switching
// projects actually leaves the one it was in, and that nothing from the old one
// is still on screen or in the query cache afterwards.
import { expect, test } from "./fixtures";
import { projectRoot } from "./helpers";

test("Z1: the switcher names the project, and switching leaves it", async ({
  page,
  appBase,
  apiBase,
}) => {
  await page.goto(appBase);
  const switcher = page.getByTestId("project-switcher");
  await expect(switcher).toContainText("Default");

  // A second project of the same account. The credential does not change —
  // only the path does, which is the whole point of the scope being the
  // project rather than the principal.
  const created = await page.request.post(`${apiBase.split("/api/p/")[0]}/api/projects`, {
    data: { name: "e2e-second" },
  });
  expect(created.status()).toBe(201);
  const second = (await created.json()) as { id: string; name: string };

  await page.reload();
  await switcher.click();
  await page.getByTestId(`switch-to-${second.id}`).click();

  await page.waitForURL((url) => url.pathname.startsWith(`/p/${second.id}`));
  await expect(page.getByTestId("project-switcher")).toContainText("e2e-second");
});

test("Z2: a second project starts empty and does not show the first's work", async ({
  page,
  appBase,
  apiBase,
}) => {
  const origin = apiBase.split("/api/p/")[0];

  // Something that exists in the default project and nowhere else.
  const cfg = (await (await page.request.get(`${apiBase}/config`)).json()) as {
    models: { alias: string }[];
  };
  const alias = cfg.models[0]?.alias;
  expect(alias, "the e2e harness seeds at least one model").toBeTruthy();
  const agent = await page.request.post(`${apiBase}/agents`, {
    data: { name: "e2e-only-here", model: alias },
  });
  expect(agent.status()).toBe(201);

  const created = await page.request.post(`${origin}/api/projects`, {
    data: { name: "e2e-empty" },
  });
  expect(created.status()).toBe(201);
  const empty = (await created.json()) as { id: string };

  // The agent is on the page in the project that owns it… By name, not by
  // "the only row": this suite shares one deployment and other groups leave
  // their own presets behind.
  const mine = page.locator('[data-agent-name="e2e-only-here"]');
  await page.goto(`${appBase}/agents`);
  await expect(mine).toBeVisible();

  // …and absent from the next one, which starts with nothing at all —
  // including no models, because credentials are not shared either.
  await page.goto(`${origin}/p/${empty.id}/agents`);
  await expect(page.getByTestId("agent-row")).toHaveCount(0);

  const theirs = (await (
    await page.request.get(`${origin}/api/p/${empty.id}/config`)
  ).json()) as { models: unknown[]; providers: unknown[] };
  expect(theirs.models).toHaveLength(0);
  expect(theirs.providers).toHaveLength(0);

  // Clean up: the suite is serial and shares one deployment, so a project left
  // behind would show up in every later switcher assertion.
  await page.request.delete(`${origin}/api/projects/${empty.id}`);
  await page.request.delete(`${apiBase}/agents/e2e-only-here`);
});

test("Z3: the default project cannot be deleted", async ({ page, appBase }) => {
  await page.goto(`${appBase}/settings/projects`);
  const rows = page.getByTestId(/^project-/);
  await expect(rows.first()).toContainText("Default");
  // Disabled rather than absent: the reason is in the tooltip, and a control
  // that vanishes teaches nothing about why.
  await expect(
    page.getByRole("button", { name: "The default project cannot be deleted" }),
  ).toBeDisabled();
});

test("Z4: a project created from settings appears in the switcher", async ({
  page,
  appBase,
  apiBase,
}) => {
  const origin = apiBase.split("/api/p/")[0];
  await page.goto(`${appBase}/settings/projects`);

  await page.getByTestId("new-project-name").fill("e2e-from-settings");
  await page.getByTestId("create-project").click();

  const row = page.locator('[data-testid^="project-"]', {
    hasText: "e2e-from-settings",
  });
  await expect(row).toBeVisible();

  const id = (await row.getAttribute("data-testid"))?.replace("project-", "");
  expect(id).toBeTruthy();

  // By its switch target, not by its text: the settings row behind the open
  // menu carries the same name.
  await page.getByTestId("project-switcher").click();
  await expect(page.getByTestId(`switch-to-${id}`)).toBeVisible();
  await page.keyboard.press("Escape");

  await page.request.delete(`${origin}/api/projects/${id}`);
});

test("Z5: the app root sends a browser to the default project", async ({
  page,
  apiBase,
}) => {
  const origin = apiBase.split("/api/p/")[0];
  await page.goto(origin);
  await page.waitForURL((url) => url.pathname.startsWith(projectRoot()));
  await expect(page.getByTestId("project-switcher")).toContainText("Default");
});
