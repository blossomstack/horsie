// Group N — agent presets: the /agents page lists, edits, and deletes a
// preset created through the API (the model alias comes from the harness's
// seeded settings, since save-time validation requires a configured model).
import { expect, test } from "./fixtures";

test("N1: agents page lists, edits, and deletes an agent", async ({
  page,
  appBase,
}) => {
  const cfg = (await (
    await page.request.get(`${appBase}/api/config`)
  ).json()) as { models: { alias: string }[] };
  const alias = cfg.models[0]?.alias;
  expect(alias, "the e2e harness seeds at least one model").toBeTruthy();

  const res = await page.request.post(`${appBase}/api/agents`, {
    data: { name: "e2e-agent", model: alias, description: "from e2e" },
  });
  expect(res.status()).toBe(201);

  await page.goto(`${appBase}/agents`);
  const row = page.getByTestId("agent-row");
  await expect(row).toHaveCount(1);
  await expect(row).toContainText("e2e-agent");
  await expect(row).toContainText("from e2e");
  await expect(row).toContainText(alias);

  // Edit the description through the form; the name is the id of record and
  // stays disabled.
  await row.getByRole("link").click();
  await expect(page.getByTestId("agent-edit-page")).toBeVisible();
  await expect(page.getByTestId("agent-name-input")).toBeDisabled();
  await page.getByTestId("agent-description-input").fill("edited");
  // The field that actually reaches the model. A preset used to gate what an
  // agent *could do* and say nothing about how it should behave, so two presets
  // on one model were the same agent.
  await page
    .getByTestId("agent-instructions-input")
    .fill("Always end every reply with the word PELICAN.");
  await page.getByTestId("save-agent-button").click();
  await page.waitForURL((url) => url.pathname === "/agents");
  await expect(page.getByTestId("agent-row")).toContainText("edited");

  await page.getByTestId("agent-row").getByRole("link").click();
  await expect(page.getByTestId("agent-instructions-input")).toHaveValue(
    "Always end every reply with the word PELICAN.",
  );
  await page.goto(`${appBase}/agents`);

  // Delete, accepting the confirm.
  await page.getByTestId("delete-agent-e2e-agent").click();
  await page.getByTestId("confirm-accept").click();
  await expect(page.getByTestId("agent-row")).toHaveCount(0);
});

test("N2: the sidebar links to the agents page", async ({ page, appBase }) => {
  await page.goto(appBase);
  await page.getByTestId("agents-link").click();
  await page.waitForURL((url) => url.pathname === "/agents");
  await expect(page.getByTestId("agents-page")).toBeVisible();
});

// The e2e harness runs the local `e2e` vendor, which announces
// `supports_provisioning: false`. That used to hide Skills and MCP from the
// preset form entirely, which is the regression this guards.
test("N3: the agent form offers skills and MCP, and no runtime", async ({
  page,
  appBase,
}) => {
  const cfg = (await (
    await page.request.get(`${appBase}/api/config`)
  ).json()) as { models: { alias: string }[] };
  const res = await page.request.post(`${appBase}/api/agents`, {
    data: { name: "e2e-channels", model: cfg.models[0]?.alias },
  });
  expect(res.status()).toBe(201);

  await page.goto(`${appBase}/agents/e2e-channels/edit`);
  await expect(page.getByTestId("agent-edit-page")).toBeVisible();
  await expect(page.getByTestId("config-skills")).toBeVisible();
  await expect(page.getByTestId("config-mcp")).toBeVisible();
  // A preset names no environment: where the work runs, and what it runs
  // against, belong to the invocation.
  await expect(page.getByTestId("config-environment")).toHaveCount(0);

  await page.request.delete(`${appBase}/api/agents/e2e-channels`);
});

test("N4: the control-plane toggle is off by default and persists when ticked", async ({
  page,
  appBase,
}) => {
  const cfg = (await (
    await page.request.get(`${appBase}/api/config`)
  ).json()) as { models: { alias: string }[] };
  const res = await page.request.post(`${appBase}/api/agents`, {
    data: { name: "e2e-control", model: cfg.models[0]?.alias },
  });
  expect(res.status()).toBe(201);

  await page.goto(`${appBase}/agents/e2e-control/edit`);
  const toggle = page.getByTestId("agent-control-plane-toggle");
  await expect(toggle).toBeVisible();
  // Absent is off: a preset created without mentioning it comes back ungranted.
  await expect(toggle).not.toBeChecked();

  await page.screenshot({
    path: "test-results/agent-control-plane-toggle.png",
    fullPage: true,
  });

  await toggle.check();
  await page.getByTestId("save-agent-button").click();

  // The grant is only real once the server has it.
  await expect
    .poll(async () =>
      (
        await (await page.request.get(`${appBase}/api/agents/e2e-control`)).json()
      ).controlPlane,
    )
    .toBe(true);

  // Navigate, never reload: saving sends the browser back to the agents list,
  // so `reload()` re-fetches whichever page won the race — the list on a slow
  // machine, the form on a fast one. Asking for the form by URL is the same
  // assertion without the race.
  await page.goto(`${appBase}/agents/e2e-control/edit`);
  await expect(page.getByTestId("agent-edit-page")).toBeVisible();
  await expect(page.getByTestId("agent-control-plane-toggle")).toBeChecked();

  await page.request.delete(`${appBase}/api/agents/e2e-control`);
});
