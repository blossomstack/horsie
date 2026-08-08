import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { REPO_ROOT, WEB_DIR, freePort, waitFor } from "./harness";

// Auth-enabled server of its own, as in n-auth-login / o-device-approval.
let proc: ChildProcess | undefined;
let baseURL = "";
let password = "";
let root = "";

test.beforeAll(async () => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "horsie-tokens-e2e-"));
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
    label: "token server /api/health",
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

test("a machine token is shown once, works as a credential, and can be revoked", async ({
  page,
}) => {
  await page.goto(`${baseURL}/settings/account`);
  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("machine-tokens")).toBeVisible();

  await page.getByTestId("token-label").fill("ci-box");
  await page.getByTestId("token-create").click();

  const secretBlock = page.getByTestId("token-secret");
  await expect(secretBlock).toBeVisible();
  const secret = (await secretBlock.locator("code").innerText()).trim();
  expect(secret).toMatch(/^hsk_agt_/);
  await expect(page.getByTestId("token-row-ci-box")).toBeVisible();

  // It authenticates.
  const ok = await fetch(`${baseURL}/api/sessions`, {
    headers: { authorization: `Bearer ${secret}` },
  });
  expect(ok.status).toBe(200);

  // The secret is not recoverable: a reload lists the token without it.
  await page.reload();
  await expect(page.getByTestId("token-row-ci-box")).toBeVisible();
  await expect(page.getByTestId("token-secret")).toHaveCount(0);

  // Revoking kills it.
  await page.getByTestId("token-revoke-ci-box").click();
  await expect(page.getByTestId("token-row-ci-box")).toHaveCount(0);
  const dead = await fetch(`${baseURL}/api/sessions`, {
    headers: { authorization: `Bearer ${secret}` },
  });
  expect(dead.status).toBe(401);
});
