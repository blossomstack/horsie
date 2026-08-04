import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { RenderedMessage } from "../hooks/useSessionStream";
import { Transcript } from "./Transcript";
import { TurnActions } from "./TurnActions";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

// jsdom has no Clipboard API; `copyText` falls back to `execCommand`, which
// jsdom also lacks, so every copy would return false and nothing would be
// observable. Stub the modern path the browser actually uses.
let writeText: ReturnType<typeof vi.fn>;

beforeEach(() => {
  writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: { writeText },
  });
});

describe("TurnActions copy on a user turn", () => {
  // A user message is already plain text, so its single copy button must take
  // the message itself — not the empty string that results when neither
  // `markdown` nor `renderedRef` is supplied.
  it("copies the plain text it is given", async () => {
    render(<TurnActions atMs={123} plainText="Hello from the user" />);
    fireEvent.click(screen.getByTestId("turn-copy-plain"));
    await vi.waitFor(() =>
      expect(writeText).toHaveBeenCalledWith("Hello from the user"),
    );
  });
});

describe("Transcript user turn", () => {
  it("copies the user's message text", async () => {
    const userMsg: RenderedMessage = {
      id: "u1",
      role: "User",
      text: "Show me the copy button",
      thinking: [],
      toolCalls: [],
      subagentResults: [],
      createdAtMs: 1_000,
    };
    render(
      <Transcript
        messages={[userMsg]}
        streaming=""
        orphanTools={[]}
        showLive={false}
        showThinking={false}
      />,
    );
    fireEvent.click(screen.getByTestId("turn-copy-plain"));
    await vi.waitFor(() =>
      expect(writeText).toHaveBeenCalledWith("Show me the copy button"),
    );
  });
});
