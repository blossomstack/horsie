// Test fixtures: `appBase` (the running server's URL) and `mock` (a thin
// client for the mock LLM control plane). Tests navigate with the absolute
// `appBase` URL and program deterministic LLM responses through `mock`.

import { test as base, expect } from "@playwright/test";
import { readRuntimeInfo } from "./harness";

export type MockResponse = (
  | { type: "text"; content: string }
  | { type: "text_stream"; chunks: string[] }
  | { type: "tool_call"; name: string; input: unknown }
  | { type: "tool_calls"; calls: [string, unknown][] }
  | { type: "error"; status: number; message: string }
  | { type: "thinking"; text: string; signature: string }
) & {
  /** Hold this answer back before sending it. The only way an out-of-process
   * test can make a turn *observably* in flight: a case that means to watch
   * something change during a turn needs the turn to still be running when it
   * looks. */
  delayMs?: number;
};

/**
 * How long a held-back first answer waits, in milliseconds.
 *
 * Long enough for the browser to navigate off the draft, mount the session view
 * and get both of its streams connected; short enough that a suite of 74 cases
 * does not notice. Only the tests that watch a turn *while it runs* pay it.
 */
export const FIRST_TURN_HOLD_MS = 400;

/** Programs the mock LLM's FIFO response queue over its control plane. */
export class MockLlm {
  constructor(private readonly url: string) {}

  private async post(pathname: string, body?: unknown): Promise<void> {
    const res = await fetch(`${this.url}${pathname}`, {
      method: "POST",
      headers: body === undefined ? {} : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!res.ok) throw new Error(`mock POST ${pathname} → ${res.status}`);
  }

  /**
   * Every request body the mock has received since the last `reset`, newest
   * first. Tests assert on what reached the agent (e.g. the composed system
   * prompt) via `capturedContains`.
   */
  async received(): Promise<unknown[]> {
    const res = await fetch(`${this.url}/received`);
    if (!res.ok) throw new Error(`mock GET /received → ${res.status}`);
    return (await res.json()) as unknown[];
  }

  /**
   * True if any captured request's JSON (system prompt included) contains
   * `needle`. Wire-agnostic: matches whether the prompt rode in the Anthropic
   * top-level `system` field or an OpenAI system message.
   */
  async capturedContains(needle: string): Promise<boolean> {
    const bodies = await this.received();
    return bodies.some((b) => JSON.stringify(b).includes(needle));
  }

  /**
   * Clear the queue + per-session state, and hold this test's first answer.
   * Call in beforeEach.
   *
   * The hold is the default because a session's first turn now starts with the
   * session itself — the create carries the first message — so with a
   * zero-latency mock the turn can begin and end while the browser is still
   * navigating to it. No client could watch that, and a real provider never
   * behaves that way. Holding the first answer restores the assumption every
   * case here was written under: the page is subscribed before anything
   * happens. Pass 0 for a test that genuinely wants the instant case.
   */
  reset(holdFirstMs: number = FIRST_TURN_HOLD_MS): Promise<void> {
    return this.post("/reset", { holdFirstMs });
  }
  queue(r: MockResponse): Promise<void> {
    return this.post("/queue", r);
  }
  queueText(content: string, delayMs?: number): Promise<void> {
    return this.queue({ type: "text", content, delayMs });
  }
  queueTextStream(chunks: string[]): Promise<void> {
    return this.queue({ type: "text_stream", chunks });
  }
  queueToolCall(name: string, input: unknown): Promise<void> {
    return this.queue({ type: "tool_call", name, input });
  }
  /** One assistant message making several calls at once (parallel tool use). */
  queueToolCalls(calls: [string, unknown][]): Promise<void> {
    return this.queue({ type: "tool_calls", calls });
  }
  queueError(status: number, message: string): Promise<void> {
    return this.queue({ type: "error", status, message });
  }
  queueThinking(text: string, signature = "sig-e2e"): Promise<void> {
    return this.queue({ type: "thinking", text, signature });
  }
}

export const test = base.extend<{
  appBase: string;
  apiBase: string;
  mock: MockLlm;
  marketplaceUrl: string;
}>({
  /**
   * The app, rooted at the account's default project.
   *
   * Every page in horsie lives under `/p/<project>`, and the router's basename
   * is that prefix — so a test that navigated to the bare origin would land on
   * the redirect rather than on the page it asked for.
   */
  appBase: async ({}, use) => {
    const { baseURL, project } = readRuntimeInfo();
    await use(`${baseURL}/p/${project}`);
  },
  /** The API, rooted at the same project. */
  apiBase: async ({}, use) => {
    const { baseURL, project } = readRuntimeInfo();
    await use(`${baseURL}/api/p/${project}`);
  },
  marketplaceUrl: async ({}, use) => {
    await use(readRuntimeInfo().marketplaceUrl);
  },
  mock: async ({}, use) => {
    await use(new MockLlm(readRuntimeInfo().mockUrl));
  },
});

export { expect };
