// Screenshot the project surfaces against a real server, so they are looked at
// before they ship. Not part of the suite: `bun scripts/shoot-projects.mjs`.
import { chromium } from "@playwright/test";
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import net from "node:net";

const REPO = path.resolve(import.meta.dirname, "../../..");
const WEB = path.resolve(import.meta.dirname, "..");
const OUT = path.join(WEB, "e2e", ".shots");

const freePort = () =>
  new Promise((resolve) => {
    const s = net.createServer();
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "horsie-shots-"));
const port = await freePort();
const base = `http://127.0.0.1:${port}`;

const configPath = path.join(tmp, "config.json");
fs.writeFileSync(
  configPath,
  JSON.stringify({
    storage: {
      state_dir: path.join(tmp, "state"),
      data_dir: path.join(tmp, "data"),
      plugins_dir: path.join(tmp, "plugins"),
    },
    auth: { mode: "off" },
  }),
);

const server = spawn(
  path.join(REPO, "target", "debug", "horsie-server"),
  [
    "--config",
    configPath,
    "--addr",
    `127.0.0.1:${port}`,
    "--web",
    path.join(WEB, "dist"),
  ],
  { stdio: ["ignore", "pipe", "pipe"] },
);
server.stdout.on("data", (b) => process.stdout.write(`[server] ${b}`));
server.stderr.on("data", (b) => process.stdout.write(`[server] ${b}`));

for (let i = 0; i < 100; i++) {
  try {
    if ((await fetch(`${base}/api/health`)).ok) break;
  } catch {
    /* not up yet */
  }
  await new Promise((r) => setTimeout(r, 200));
}

// A second project, so the switcher has something to switch to.
const projects = await (await fetch(`${base}/api/projects`)).json();
const home = projects.find((p) => p.isDefault) ?? projects[0];
await fetch(`${base}/api/projects`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ name: "acme-migration" }),
});

fs.mkdirSync(OUT, { recursive: true });
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });

await page.goto(`${base}/p/${home.id}/`);
await page.getByTestId("project-switcher").waitFor();
await page.screenshot({ path: path.join(OUT, "rail.png") });

await page.getByTestId("project-switcher").click();
await page.waitForTimeout(300);
await page.screenshot({ path: path.join(OUT, "switcher-open.png") });
await page.keyboard.press("Escape");

await page.goto(`${base}/p/${home.id}/settings/projects`);
await page.getByTestId("create-project").waitFor();
await page.screenshot({ path: path.join(OUT, "settings-projects.png") });

await browser.close();
server.kill("SIGKILL");
fs.rmSync(tmp, { recursive: true, force: true });
console.log(`shots in ${OUT}`);
