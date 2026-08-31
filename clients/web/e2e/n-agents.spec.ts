import { projectRoot, sendMessage } from "./helpers";
// Group N — agent presets: the /agents page lists, edits, and deletes a
// preset created through the API (the model alias comes from the harness's
// seeded settings, since save-time validation requires a configured model).
import { expect, test } from "./fixtures";

test("N1: agents page lists, edits, and deletes an agent", async ({
  page,
  appBase,
  apiBase,
}) => {
  const cfg = (await (
    await page.request.get(`${apiBase}/config`)
  ).json()) as { models: { alias: string }[] };
  const alias = cfg.models[0]?.alias;
  expect(alias, "the e2e harness seeds at least one model").toBeTruthy();

  const res = await page.request.post(`${apiBase}/agents`, {
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
  // Opting in has to be an act, so the box starts clear on a preset created
  // through the API without mentioning tuning.
  await expect(page.getByTestId("agent-tunable")).not.toBeChecked();
  await page.getByTestId("agent-tunable").check();
  await page.getByTestId("save-agent-button").click();
  await page.waitForURL((url) => url.pathname === projectRoot() + "/agents");
  await expect(page.getByTestId("agent-row")).toContainText("edited");

  await page.getByTestId("agent-row").getByRole("link").click();
  await expect(page.getByTestId("agent-instructions-input")).toHaveValue(
    "Always end every reply with the word PELICAN.",
  );
  // Round-tripped through the API and back into the form. `PUT` is a full
  // replace, so a flag the form failed to carry would come back off.
  await expect(page.getByTestId("agent-tunable")).toBeChecked();
  await page.goto(`${appBase}/agents`);

  // Delete, accepting the confirm.
  await page.getByTestId("delete-agent-e2e-agent").click();
  await page.getByTestId("confirm-accept").click();
  await expect(page.getByTestId("agent-row")).toHaveCount(0);
});

/**
 * Item 2, end to end: the preset a session was invoked with has to survive the
 * round trip and come back on the agent document, or the created session can
 * only be drawn as the settings the preset happened to expand into.
 *
 * The whole path is under test here in a way no unit test reaches: the server
 * records `AgentSource::Preset`, `GET /agents/:aid` reports it, and the frozen
 * row collapses to it.
 */
test("N7: a session invoked from a preset reports the preset, not its settings", async ({
  page,
  appBase,
  apiBase,
  mock,
}) => {
  const cfg = (await (
    await page.request.get(`${apiBase}/config`)
  ).json()) as { models: { alias: string }[] };
  const alias = cfg.models[0]?.alias;
  const res = await page.request.post(`${apiBase}/agents`, {
    data: { name: "e2e-invoked", model: alias, description: "invoked by e2e" },
  });
  expect(res.status()).toBe(201);

  await mock.queueText("on it");
  await page.goto(appBase);
  // A preset is chosen from the Model key, as the mutually-exclusive
  // alternative to naming a model directly.
  await page.getByTestId("config-model").click();
  await page
    .locator('[data-testid="agent-option"][data-value="e2e-invoked"]')
    .click();
  await expect(page.getByTestId("config-model")).toHaveAttribute(
    "aria-label",
    "Model — e2e-invoked",
  );
  await sendMessage(page, "hello from a preset");

  const bar = page.getByTestId("session-config-bar");
  await expect(bar).toHaveAttribute("data-mode", "locked");
  await expect(page.getByTestId("config-model")).toHaveAttribute(
    "aria-label",
    "Model — e2e-invoked",
  );
  // The channels the preset supplied are not separate decisions, so the row
  // does not redraw them as separate keys.
  await expect(page.getByTestId("config-mcp")).toHaveCount(0);
  await expect(page.getByTestId("config-memory")).toHaveCount(0);

  // Collapsing must not hide: the settings are inside the one key.
  await page.getByTestId("config-model").click();
  await expect(page.getByTestId("resolved-model")).toHaveText(alias!);

  await page.request.delete(`${apiBase}/agents/e2e-invoked`);
});

test("N2: the sidebar links to the agents page", async ({ page, appBase }) => {
  await page.goto(appBase);
  await page.getByTestId("agents-link").click();
  await page.waitForURL((url) => url.pathname === projectRoot() + "/agents");
  await expect(page.getByTestId("agents-page")).toBeVisible();
});

// The e2e harness runs the local `e2e` vendor, which announces
// `supports_provisioning: false`. That used to hide Skills and MCP from the
// preset form entirely, which is the regression this guards.
test("N3: the agent form offers skills and MCP, and no runtime", async ({
  page,
  appBase,
  apiBase,
}) => {
  const cfg = (await (
    await page.request.get(`${apiBase}/config`)
  ).json()) as { models: { alias: string }[] };
  const res = await page.request.post(`${apiBase}/agents`, {
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

  await page.request.delete(`${apiBase}/agents/e2e-channels`);
});

test("N4: the Horsie tool group is the control-plane grant, and it persists", async ({
  page,
  appBase,
  apiBase,
}) => {
  const cfg = (await (
    await page.request.get(`${apiBase}/config`)
  ).json()) as { models: { alias: string }[] };
  const res = await page.request.post(`${apiBase}/agents`, {
    data: { name: "e2e-control", model: cfg.models[0]?.alias },
  });
  expect(res.status()).toBe(201);

  await page.goto(`${appBase}/agents/e2e-control/edit`);
  // The standalone checkbox is gone: the grant is a tool selection now, so a
  // second control beside the list could disagree with it.
  await expect(page.getByTestId("agent-control-plane-toggle")).toHaveCount(0);

  // The form renders each channel as a labelled field, not the session row's
  // icon-only key, so the value is visible text rather than an accessible name.
  const tools = page.getByTestId("config-tools");
  await expect(tools).toContainText("Default");
  await tools.click();

  // Absent is ungranted: a preset created without mentioning tools comes back
  // with the control plane untouched. Read off the group row, which is what
  // the picker shows before anything is opened.
  const horsieGroup = page.getByTestId("tool-group-all-control");
  await expect(horsieGroup).not.toBeChecked();

  await page.screenshot({
    path: "test-results/agent-tools-picker.png",
    fullPage: true,
  });

  // Ticking the group is the grant — no need to open it.
  await horsieGroup.check();
  await page.keyboard.press("Escape");
  await page.getByTestId("save-agent-button").click();

  // The grant is only real once the server has it.
  await expect
    .poll(async () =>
      (
        await (await page.request.get(`${apiBase}/agents/e2e-control`)).json()
      ).allowedTools?.includes("horsie_agents"),
    )
    .toBe(true);

  // Navigate, never reload: saving sends the browser back to the agents list,
  // so `reload()` re-fetches whichever page won the race — the list on a slow
  // machine, the form on a fast one. Asking for the form by URL is the same
  // assertion without the race.
  await page.goto(`${appBase}/agents/e2e-control/edit`);
  await expect(page.getByTestId("agent-edit-page")).toBeVisible();
  await page.getByTestId("config-tools").click();
  await expect(page.getByTestId("tool-group-all-control")).toBeChecked();

  await page.request.delete(`${apiBase}/agents/e2e-control`);
});
