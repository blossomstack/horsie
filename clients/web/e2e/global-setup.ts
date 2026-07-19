// Playwright global-setup: bring up the full system under test —
//   mock LLM  ←  horsie-server (real, --web dist)  ←  horsie-runtime daemon
// — seed the settings DB so the UI has a model + the local `e2e` vendor, then
// hand the URLs to the tests via a runtime file. Mirrors the validated
// backend flow; only the driver (a browser) is added on top.

import { spawn, execFileSync, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  REPO_ROOT,
  WEB_DIR,
  RUNTIME_FILE,
  freePort,
  waitFor,
  putConfig,
  type RuntimeInfo,
} from "./harness";

const log = (m: string) => console.log(`[e2e:setup] ${m}`);

export default async function globalSetup(): Promise<void> {
  const skipBuild = process.env.HORSIE_E2E_SKIP_BUILD === "1";
  const binDir = path.join(REPO_ROOT, "target", "debug");
  const serverBin = path.join(binDir, "horsie-server");
  const runtimeBin = path.join(binDir, "horsie-runtime");
  const mockBin = path.join(binDir, "horsie-mock-llm");
  const distDir = path.join(WEB_DIR, "dist");

  if (!skipBuild) {
    log("building rust binaries (horsie-server, horsie-runtime, horsie-mock-llm)…");
    execFileSync(
      "cargo",
      ["build", "-p", "horsie-server", "-p", "horsie-runtime", "-p", "horsie-mock-llm"],
      { cwd: REPO_ROOT, stdio: "inherit" },
    );
    log("building web assets (bun run build)…");
    execFileSync("bun", ["run", "build"], { cwd: WEB_DIR, stdio: "inherit" });
  }
  for (const b of [serverBin, runtimeBin, mockBin]) {
    if (!fs.existsSync(b)) {
      throw new Error(`missing binary ${b} — build first, or unset HORSIE_E2E_SKIP_BUILD`);
    }
  }
  if (!fs.existsSync(path.join(distDir, "index.html"))) {
    throw new Error(`missing web build at ${distDir} — run 'bun run build'`);
  }

  const mockPort = await freePort();
  const serverPort = await freePort();
  const baseURL = `http://127.0.0.1:${serverPort}`;
  const mockUrl = `http://127.0.0.1:${mockPort}`;

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "horsie-e2e-"));
  const scratch = path.join(tmpDir, "scratch");
  fs.mkdirSync(scratch, { recursive: true });
  const configPath = path.join(tmpDir, "config.json");
  fs.writeFileSync(
    configPath,
    JSON.stringify({
      local_runtime: true,
      storage: {
        state_dir: path.join(tmpDir, "state"),
        data_dir: path.join(tmpDir, "data"),
        plugins_dir: path.join(tmpDir, "plugins"),
      },
    }),
  );

  const children: ChildProcess[] = [];
  const spawnProc = (bin: string, args: string[], logName: string): ChildProcess => {
    const out = fs.openSync(path.join(tmpDir, logName), "a");
    const child = spawn(bin, args, { stdio: ["ignore", out, out] });
    children.push(child);
    return child;
  };
  const dumpLogs = () => {
    for (const name of ["mock.log", "server.log", "runtime.log"]) {
      const p = path.join(tmpDir, name);
      if (fs.existsSync(p)) {
        log(`----- ${name} -----\n${fs.readFileSync(p, "utf8")}`);
      }
    }
  };

  try {
    log(`starting mock-llm on ${mockUrl}`);
    spawnProc(mockBin, ["--port", String(mockPort)], "mock.log");

    log(`starting horsie-server on ${baseURL} (--web ${distDir})`);
    spawnProc(
      serverBin,
      ["--config", configPath, "--addr", `127.0.0.1:${serverPort}`, "--web", distDir],
      "server.log",
    );
    await waitFor(async () => (await fetch(`${baseURL}/api/health`)).ok, {
      timeoutMs: 30_000,
      label: "server /api/health",
    });

    log("seeding provider 'mock' + model 'mock-sonnet'");
    await putConfig(baseURL, {
      providers: [{ name: "mock", kind: "anthropic", baseUrl: mockUrl, apiKey: "test-key" }],
      models: [{ alias: "mock-sonnet", provider: "mock", modelId: "mock-model", maxTokens: 4096 }],
    });

    log("starting horsie-runtime daemon (runtime-id 'e2e')");
    spawnProc(
      runtimeBin,
      [
        "--endpoint",
        `ws://127.0.0.1:${serverPort}/api/runtime/connect?register=local`,
        "--runtime-id",
        "e2e",
        "--workspace",
        `main=${scratch}`,
      ],
      "runtime.log",
    );
    await waitFor(
      async () => {
        const cfg = (await (await fetch(`${baseURL}/api/config`)).json()) as {
          vendors?: { name: string; active: boolean }[];
        };
        return (cfg.vendors ?? []).some((v) => v.name === "e2e" && v.active);
      },
      { timeoutMs: 20_000, label: "vendor 'e2e' active" },
    );

    log("setting defaultVendor=e2e");
    await putConfig(baseURL, { defaultVendor: "e2e" });

    const info: RuntimeInfo = {
      baseURL,
      mockUrl,
      tmpDir,
      pids: children.map((c) => c.pid).filter((p): p is number => typeof p === "number"),
    };
    fs.writeFileSync(RUNTIME_FILE, JSON.stringify(info, null, 2));
    log(`ready → app=${baseURL} mock=${mockUrl} tmp=${tmpDir}`);
  } catch (err) {
    log(`setup failed: ${(err as Error).message}`);
    dumpLogs();
    for (const c of children) {
      if (typeof c.pid === "number") {
        try {
          process.kill(c.pid, "SIGKILL");
        } catch {
          // already gone
        }
      }
    }
    throw err;
  }
}
