// Playwright global-teardown: kill every process global-setup spawned and
// remove the temp root. Best-effort — never throws, so a failing suite still
// tears down cleanly.

import fs from "node:fs";
import { RUNTIME_FILE, readRuntimeInfo, sleep } from "./harness";

export default async function globalTeardown(): Promise<void> {
  if (!fs.existsSync(RUNTIME_FILE)) return;
  let info;
  try {
    info = readRuntimeInfo();
  } catch {
    return;
  }

  const signal = (sig: NodeJS.Signals) => {
    for (const pid of info.pids) {
      try {
        process.kill(pid, sig);
      } catch {
        // already exited
      }
    }
  };

  signal("SIGTERM");
  await sleep(500);
  signal("SIGKILL");

  try {
    fs.rmSync(info.tmpDir, { recursive: true, force: true });
  } catch {
    // leave it for the OS temp reaper
  }
  try {
    fs.rmSync(RUNTIME_FILE);
  } catch {
    // ignore
  }
}
