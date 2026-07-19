import { defineConfig, devices } from "@playwright/test";

// The suite drives a real horsie-server + mock LLM + real local runtime daemon
// (brought up in global-setup) through the browser. Responses are programmed
// via the mock's global FIFO queue, so tests MUST run serially — one worker,
// no parallelism — to keep the queue deterministic.
export default defineConfig({
  testDir: "./e2e",
  outputDir: "./e2e/.output",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 45_000,
  expect: { timeout: 15_000 },
  reporter: [["list"]],
  globalSetup: "./e2e/global-setup.ts",
  globalTeardown: "./e2e/global-teardown.ts",
  use: {
    actionTimeout: 15_000,
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
