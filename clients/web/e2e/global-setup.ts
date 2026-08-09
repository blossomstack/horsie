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
  seedConfig,
  setDefaultRuntimeVendor,
  type RuntimeInfo,
} from "./harness";

const log = (m: string) => console.log(`[e2e:setup] ${m}`);

export default async function globalSetup(): Promise<void> {
  const skipBuild = process.env.HORSIE_E2E_SKIP_BUILD === "1";
  const binDir = path.join(REPO_ROOT, "target", "debug");
  const serverBin = path.join(binDir, "horsie-server");
  const runtimeBin = path.join(binDir, "horsie-runtime");
  // The vendor process: sessions reach this machine only through it now.
  const cliBin = path.join(binDir, "horsie");
  const mockBin = path.join(binDir, "async-llm-mock");
  const distDir = path.join(WEB_DIR, "dist");

  if (!skipBuild) {
    log("building rust binaries (horsie-server, horsie, horsie-runtime, async-llm-mock)…");
    execFileSync(
      "cargo",
      ["build", "-p", "horsie-server", "-p", "horsie", "-p", "horsie-runtime"],
      { cwd: REPO_ROOT, stdio: "inherit" },
    );
    // The mock server is async-llm's own binary. It builds only because a
    // workspace member takes async-llm as a normal dependency with `mock` on —
    // a dev-dependency's features are not active for `cargo build`.
    execFileSync(
      "cargo",
      ["build", "-p", "async-llm", "--bin", "async-llm-mock"],
      { cwd: REPO_ROOT, stdio: "inherit" },
    );
    log("building web assets (bun run build)…");
    execFileSync("bun", ["run", "build"], { cwd: WEB_DIR, stdio: "inherit" });
  }
  for (const b of [serverBin, cliBin, runtimeBin, mockBin]) {
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

  // Workspace context the runtime's ScanWorkspace must surface into the agent's
  // system prompt (group F): a project instruction file and one workspace skill.
  fs.writeFileSync(
    path.join(scratch, "AGENTS.md"),
    "E2E_AGENTS_MARKER: follow the house rules for this workspace.\n",
  );
  const wsSkillDir = path.join(scratch, ".claude", "skills", "e2e-skill");
  fs.mkdirSync(wsSkillDir, { recursive: true });
  fs.writeFileSync(
    path.join(wsSkillDir, "SKILL.md"),
    "---\nname: e2e-skill\ndescription: E2E_SKILL_DESC exercise the workspace skill loader\n---\nDo the workspace thing.\n",
  );

  // A bundle installed into the server and marked default-for-new-sessions, so
  // every session's runtime fetches it (group F): one plugin providing a shared
  // skill and a SessionStart hook whose stdout becomes the agent's
  // `# Session bootstrap` block.
  const pluginRepo = path.join(tmpDir, "e2e-plugin");
  const sharedSkillDir = path.join(pluginRepo, "skills", "e2e-shared-skill");
  fs.mkdirSync(sharedSkillDir, { recursive: true });
  fs.writeFileSync(
    path.join(sharedSkillDir, "SKILL.md"),
    "---\nname: e2e-shared-skill\ndescription: E2E_SHARED_DESC exercise the shared skill loader\n---\nDo the shared thing.\n",
  );
  const hooksDir = path.join(pluginRepo, "hooks");
  fs.mkdirSync(hooksDir, { recursive: true });
  fs.writeFileSync(
    path.join(hooksDir, "hooks.json"),
    JSON.stringify({
      hooks: {
        SessionStart: [
          { hooks: [{ type: "command", command: "echo E2E_BOOTSTRAP_MARKER" }] },
        ],
        // A tool-scoped record alongside the standalone one, so both transcript
        // renderings come from one fixture. `systemMessage` is addressed to the
        // user, which is what group T asserts survives a reload.
        PostToolUse: [
          {
            matcher: "Bash",
            hooks: [
              {
                type: "command",
                command: `printf '{"systemMessage":"E2E_HOOK_NOTE"}'`,
                timeout: 5,
              },
            ],
          },
        ],
      },
    }),
  );

  fs.mkdirSync(path.join(pluginRepo, ".claude-plugin"), { recursive: true });
  fs.writeFileSync(
    path.join(pluginRepo, ".claude-plugin", "plugin.json"),
    JSON.stringify({
      name: "e2e-plugin",
      version: "0.1.0",
      description: "E2E fixture: a shared skill and two hooks",
    }),
  );
  const gitInit = (dir: string) => {
    const run = (args: string[]) =>
      execFileSync("git", ["-C", dir, ...args], { stdio: "ignore" });
    run(["init", "-q"]);
    run(["config", "user.email", "e2e@example.com"]);
    run(["config", "user.name", "e2e"]);
    run(["add", "-A"]);
    run(["commit", "-qm", "init"]);
  };
  gitInit(pluginRepo);
  const pluginUrl = `file://${pluginRepo}`;

  // A local git marketplace so group U can exercise the real ingest path —
  // clone, parse the index, resolve an entry, pack — without the network.
  const marketDir = path.join(tmpDir, "market");
  for (const [entry, skill] of [
    ["e2e-alpha", "e2e-alpha-skill"],
    ["e2e-beta", "e2e-beta-skill"],
  ]) {
    const d = path.join(marketDir, "plugins", entry, "skills", skill);
    fs.mkdirSync(d, { recursive: true });
    fs.writeFileSync(
      path.join(d, "SKILL.md"),
      `---\nname: ${skill}\ndescription: E2E marketplace fixture skill\n---\nbody\n`,
    );
  }
  fs.mkdirSync(path.join(marketDir, ".claude-plugin"), { recursive: true });
  fs.writeFileSync(
    path.join(marketDir, ".claude-plugin", "marketplace.json"),
    JSON.stringify({
      name: "e2e-market",
      plugins: [
        {
          name: "e2e-alpha",
          description: "the first fixture plugin",
          source: "./plugins/e2e-alpha",
        },
        { name: "e2e-beta", description: "the second", source: "./plugins/e2e-beta" },
      ],
    }),
  );
  gitInit(marketDir);
  const marketplaceUrl = `file://${marketDir}`;

  const configPath = path.join(tmpDir, "config.json");
  fs.writeFileSync(
    configPath,
    JSON.stringify({
      storage: {
        state_dir: path.join(tmpDir, "state"),
        data_dir: path.join(tmpDir, "data"),
        plugins_dir: path.join(tmpDir, "plugins"),
      },
      // The suite drives the API and the UI without signing in. Authentication
      // has its own spec, which brings up its own server with it enabled.
      auth: { mode: "off" },
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

    log("seeding providers 'mock' (anthropic) + 'mock-openai' (openai) and their models");
    await seedConfig(baseURL, {
      providers: [
        { name: "mock", kind: "anthropic", baseUrl: mockUrl, apiKey: "test-key" },
        // Same mock server, OpenAI wire — the provider appends /v1/chat/completions.
        { name: "mock-openai", kind: "openai", baseUrl: mockUrl, apiKey: "test-key" },
      ],
      models: [
        // A context window is seeded so the header's context gauge has a real
        // denominator to draw an arc from; without one it can only render its
        // "window unknown" state and the dial is never exercised.
        {
          alias: "mock-sonnet",
          provider: "mock",
          modelId: "mock-model",
          maxTokens: 4096,
          contextWindow: 200000,
        },
        // Alias sorts AFTER "mock-sonnet" (models are ORDER BY alias) so the
        // New Session modal's default (models[0]) stays the Anthropic wire —
        // the OpenAI wire is opt-in per test via createSession({ model }).
        { alias: "openai-mock", provider: "mock-openai", modelId: "mock-model", maxTokens: 4096 },
      ],
    });

    // Install the fixture bundle. Deliberately NOT marked default-for-new-
    // sessions: a selected bundle is fetched and unpacked by the runtime before
    // the session can take a turn, and paying that on every spec both slows the
    // suite and makes the composer's send-while-starting race reachable. The
    // specs that assert on plugin content ask for it via
    // `createSession(..., { skills: ["e2e-plugin"] })`.
    log("installing the e2e-plugin bundle");
    const installed = await fetch(`${baseURL}/api/plugins`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sourceUrl: pluginUrl }),
    });
    if (!installed.ok) {
      throw new Error(`installing e2e-plugin: ${installed.status} ${await installed.text()}`);
    }

    // `horsie connect` is the vendor process: it dials the server and spawns one
    // `horsie-runtime` per session. Its skills are the bundles the server hands
    // each session, which the runtime fetches itself.
    const connectConfig = path.join(scratch, "..", "horsie-connect.json");
    fs.writeFileSync(
      connectConfig,
      JSON.stringify({
        runtime: { bin: runtimeBin },
        storage: {
          state_dir: path.join(path.dirname(connectConfig), "connect-state"),
        },
      }),
    );
    log("starting horsie connect (vendor 'e2e')");
    spawnProc(
      cliBin,
      [
        "connect",
        "--server",
        `http://127.0.0.1:${serverPort}`,
        "--name",
        "e2e",
        "--workspace",
        `main=${scratch}`,
        "--config",
        connectConfig,
      ],
      "connect.log",
    );
    await waitFor(
      async () => {
        const cfg = (await (await fetch(`${baseURL}/api/config`)).json()) as {
          vendors?: { name: string }[];
        };
        // Every listed vendor is a connected agent — there is no inactive state.
        return (cfg.vendors ?? []).some((v) => v.name === "e2e");
      },
      { timeoutMs: 20_000, label: "vendor 'e2e' connected" },
    );

    log("setting defaultRuntimeVendor=e2e");
    await setDefaultRuntimeVendor(baseURL, "e2e");

    const info: RuntimeInfo = {
      baseURL,
      mockUrl,
      tmpDir,
      marketplaceUrl,
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
