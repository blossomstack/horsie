// Test fixtures: `appBase` (the running server's URL) and `mock` (a thin
// client for the mock LLM control plane). Tests navigate with the absolute
// `appBase` URL and program deterministic LLM responses through `mock`.

import { test as base, expect } from "@playwright/test";
import { readRuntimeInfo } from "./harness";

export type MockResponse =
  | { type: "text"; content: string }
  | { type: "text_stream"; chunks: string[] }
  | { type: "tool_call"; name: string; input: unknown }
  | { type: "error"; status: number; message: string }
  | { type: "thinking"; text: string; signature: string };

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

  /** Clear the queue + per-session state. Call in beforeEach. */
  reset(): Promise<void> {
    return this.post("/reset");
  }
  queue(r: MockResponse): Promise<void> {
    return this.post("/queue", r);
  }
  queueText(content: string): Promise<void> {
    return this.queue({ type: "text", content });
  }
  queueTextStream(chunks: string[]): Promise<void> {
    return this.queue({ type: "text_stream", chunks });
  }
  queueToolCall(name: string, input: unknown): Promise<void> {
    return this.queue({ type: "tool_call", name, input });
  }
  queueError(status: number, message: string): Promise<void> {
    return this.queue({ type: "error", status, message });
  }
  queueThinking(text: string, signature = "sig-e2e"): Promise<void> {
    return this.queue({ type: "thinking", text, signature });
  }
}

export const test = base.extend<{ appBase: string; mock: MockLlm }>({
  appBase: async ({}, use) => {
    await use(readRuntimeInfo().baseURL);
  },
  mock: async ({}, use) => {
    await use(new MockLlm(readRuntimeInfo().mockUrl));
  },
});

export { expect };
