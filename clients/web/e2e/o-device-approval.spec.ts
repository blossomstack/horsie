import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { REPO_ROOT, WEB_DIR, freePort, waitFor } from "./harness";

// A second server with authentication ON, as in n-auth-login: the shared one
// from global-setup runs auth-disabled for every other spec.
let proc: ChildProcess | undefined;
let baseURL = "";
let password = "";
let root = "";

test.beforeAll(async () => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "horsie-device-e2e-"));
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
  const out = fs.openSync(path.join(root, "server.log"), "a");
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
    label: "device server /api/health",
  });
  const pwFile = path.join(root, "state", "server", "initial-admin-password");
  await waitFor(async () => fs.existsSync(pwFile), {
    timeoutMs: 10_000,
    label: "initial-admin-password file",
  });
  password = fs.readFileSync(pwFile, "utf8").trim();
});

test.afterAll(() => {
  proc?.kill("SIGKILL");
  fs.rmSync(root, { recursive: true, force: true });
});

test("approving a device code in the browser lets the waiting CLI collect tokens", async ({
  page,
}) => {
  // What `horsie auth login` does first.
  const started = await (
    await fetch(`${baseURL}/api/device/auth/code`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    })
  ).json();
  expect(started.userCode).toMatch(/^[A-Z0-9]{4}-[A-Z0-9]{4}$/);

  // The human opens the link, logs in, and lands on the approval page with the
  // code already filled in.
  await page.goto(`${baseURL}/auth/device?code=${started.userCode}`);
  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("device-page")).toBeVisible();
  await expect(page.getByTestId("device-code")).toHaveValue(started.userCode);
  await page.getByTestId("device-approve").click();
  await expect(page.getByTestId("device-approved")).toBeVisible();

  // The CLI's next poll gets its tokens. An approved code skips the poll
  // floor, so there is nothing to wait for.
  const res = await fetch(`${baseURL}/api/device/auth/token`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ deviceCode: started.deviceCode }),
  });
  expect(res.status).toBe(200);
  const pair = await res.json();
  expect(pair.accessToken).toMatch(/^hsk_usr_/);

  // And that token opens the API.
  const sessions = await fetch(`${baseURL}/api/sessions`, {
    headers: { authorization: `Bearer ${pair.accessToken}` },
  });
  expect(sessions.status).toBe(200);
});

test("denying a code refuses the waiting CLI", async ({ page }) => {
  const started = await (
    await fetch(`${baseURL}/api/device/auth/code`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    })
  ).json();

  await page.goto(`${baseURL}/auth/device?code=${started.userCode}`);
  // Already logged in from the previous test's session cookie? Log in if not.
  if (await page.getByTestId("login-form").isVisible().catch(() => false)) {
    await page.getByTestId("login-password").fill(password);
    await page.getByTestId("login-submit").click();
  }
  await page.getByTestId("device-deny").click();
  await expect(page.getByTestId("device-denied")).toBeVisible();

  const res = await fetch(`${baseURL}/api/device/auth/token`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ deviceCode: started.deviceCode }),
  });
  expect(res.status).toBe(400);
  expect((await res.json()).code).toBe("access_denied");
});
