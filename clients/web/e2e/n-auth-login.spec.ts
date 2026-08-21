import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { REPO_ROOT, WEB_DIR, freePort, waitFor } from "./harness";
import { projectOf } from "./helpers";

// A second server with authentication ON, on its own state/data dirs: the
// shared one from global-setup runs auth-disabled for every other spec, which
// drives the API without signing in.
let proc: ChildProcess | undefined;
let baseURL = "";
let password = "";
let root = "";

test.beforeAll(async () => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "horsie-auth-e2e-"));
  const port = await freePort();
  baseURL = `http://127.0.0.1:${port}`;

  const configPath = path.join(root, "config.json");
  fs.writeFileSync(
    configPath,
    JSON.stringify({
      storage: {
        state_dir: path.join(root, "state"),
        data_dir: path.join(root, "data"),
      },
      auth: { mode: "password" },
    }),
  );

  const logPath = path.join(root, "server.log");
  const out = fs.openSync(logPath, "a");
  proc = spawn(
    path.join(REPO_ROOT, "target", "debug", "horsie-server"),
    [
      "--config",
      configPath,
      "--addr",
      `127.0.0.1:${port}`,
      "--web",
      path.join(WEB_DIR, "dist"),
    ],
    { stdio: ["ignore", out, out] },
  );

  await waitFor(async () => (await fetch(`${baseURL}/api/health`)).ok, {
    timeoutMs: 30_000,
    label: "auth server /api/health",
  });

  // The generated password is printed once at first boot; read it from the
  // recovery file the server writes alongside it.
  const pwFile = path.join(root, "state", "server", "initial-admin-password");
  await waitFor(async () => fs.existsSync(pwFile), {
    timeoutMs: 10_000,
    label: "initial-admin-password file",
  });
  password = fs.readFileSync(pwFile, "utf8").trim();
  expect(password).toHaveLength(24);
});

test.afterAll(() => {
  proc?.kill("SIGKILL");
  fs.rmSync(root, { recursive: true, force: true });
});

test("an unauthenticated browser gets the login form, and the right password opens the app", async ({
  page,
}) => {
  await page.goto(baseURL);
  await expect(page.getByTestId("login-form")).toBeVisible();

  await page.getByTestId("login-password").fill("definitely-wrong");
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("login-error")).toBeVisible();

  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("login-form")).toHaveCount(0);
});

test("signing out returns the login form", async ({ page }) => {
  const project = await projectOf(page, baseURL, password);
  await page.goto(`${baseURL}/p/${project}/settings/account`);
  await expect(page.getByTestId("account-must-change")).toBeVisible();
  await page.getByTestId("logout").click();
  await expect(page.getByTestId("login-form")).toBeVisible();
});
