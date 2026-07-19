// Shared utilities for the web-UI e2e harness: filesystem paths, a free-port
// allocator, a generic poll-until helper, the config-seeding call, and the
// handoff file that carries the running SUT's URLs from global-setup to the
// tests and global-teardown.

import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

const E2E_DIR = path.dirname(fileURLToPath(import.meta.url)); // clients/web/e2e
export const WEB_DIR = path.resolve(E2E_DIR, ".."); // clients/web
export const REPO_ROOT = path.resolve(WEB_DIR, "../.."); // repo root
export const RUNTIME_FILE = path.join(E2E_DIR, ".runtime.json");

/** Written by global-setup, read by fixtures + global-teardown. */
export interface RuntimeInfo {
  /** Base URL of the real horsie-server (serves API + web UI same-origin). */
  baseURL: string;
  /** Base URL of the mock LLM control plane (/queue, /reset). */
  mockUrl: string;
  /** Temp root holding config, storage, scratch workspace, and process logs. */
  tmpDir: string;
  /** PIDs of the spawned processes (mock, server, runtime) to kill at teardown. */
  pids: number[];
}

export function readRuntimeInfo(): RuntimeInfo {
  return JSON.parse(fs.readFileSync(RUNTIME_FILE, "utf8")) as RuntimeInfo;
}

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Reserve an ephemeral TCP port and immediately release it. */
export function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const port = (srv.address() as net.AddressInfo).port;
      srv.close(() => resolve(port));
    });
  });
}

/** Poll `fn` until it returns true or the timeout elapses (then throw). */
export async function waitFor(
  fn: () => Promise<boolean>,
  opts: { timeoutMs: number; label: string; intervalMs?: number },
): Promise<void> {
  const { timeoutMs, label, intervalMs = 200 } = opts;
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      if (await fn()) return;
    } catch {
      // keep polling — the server may not be up yet
    }
    await sleep(intervalMs);
  }
  throw new Error(`timed out after ${timeoutMs}ms waiting for: ${label}`);
}

/** PUT /api/config with a partial SettingsUpdate; throws on non-2xx. */
export async function putConfig(baseURL: string, body: unknown): Promise<void> {
  const res = await fetch(`${baseURL}/api/config`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    throw new Error(`PUT /api/config → ${res.status}: ${await res.text()}`);
  }
}
