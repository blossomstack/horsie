import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { HookRecord } from "../api/types";
import type { RenderedToolCall } from "../hooks/useSessionStream";
import { ToolCallCard } from "./ToolCallCard";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

function hook(overrides: Partial<HookRecord> = {}): HookRecord {
  return {
    plugin: "guard",
    event: "PreToolUse",
    tool: "bash",
    toolCallId: "tc1",
    durationMs: 4,
    blocked: false,
    failed: false,
    ...overrides,
  };
}

function call(overrides: Partial<RenderedToolCall> = {}): RenderedToolCall {
  return {
    id: "tc1",
    name: "bash",
    input: { command: "rm -rf /" },
    running: false,
    hooks: [],
    ...overrides,
  };
}

describe("ToolCallCard hook records", () => {
  it("says nothing about hooks when none ran", () => {
    render(<ToolCallCard call={call()} />);
    expect(screen.queryByTestId("tool-call-hook-blocked")).toBeNull();
    fireEvent.click(screen.getByTestId("tool-call-toggle"));
    expect(screen.queryByTestId("tool-call-hooks")).toBeNull();
  });

  // A denial changes what the row means — the agent asked for something that
  // never ran — so it cannot be hidden behind the toggle.
  it("names the blocking plugin on the collapsed row", () => {
    render(
      <ToolCallCard
        call={call({
          hooks: [hook({ blocked: true, reason: "writes are not allowed" })],
        })}
      />,
    );
    expect(screen.getByTestId("tool-call-hook-blocked").textContent).toContain(
      "Blocked by guard",
    );
  });

  // "A guard ran and allowed this" is part of the audit trail, but it is detail
  // rather than state, so it lives inside the expanded card.
  it("lists every hook that ran once expanded, allowed ones included", () => {
    render(
      <ToolCallCard
        call={{
          ...call(),
          hooks: [
            hook(),
            hook({ plugin: "linter", event: "PostToolUse" }),
            hook({
              plugin: "redactor",
              event: "PostToolUse",
              outputBefore: "secret",
              outputAfter: "***",
            }),
          ],
        }}
      />,
    );
    expect(screen.queryByTestId("tool-call-hook-blocked")).toBeNull();
    fireEvent.click(screen.getByTestId("tool-call-toggle"));
    const rows = screen.getAllByTestId("tool-call-hook");
    expect(rows).toHaveLength(3);
    expect(rows[0].textContent).toContain("allowed");
    expect(rows[2].textContent).toContain("rewrote the output");
  });

  // A hook that could not run denies the call on PreToolUse, so it must read as
  // an intervention rather than as a hook that quietly passed.
  it("treats a hook that failed to run as an intervention", () => {
    render(
      <ToolCallCard call={call({ hooks: [hook({ failed: true })] })} />,
    );
    expect(screen.getByTestId("tool-call-hook-blocked").textContent).toContain(
      "Blocked by guard",
    );
    fireEvent.click(screen.getByTestId("tool-call-toggle"));
    expect(screen.getByTestId("tool-call-hook").textContent).toContain(
      "could not run",
    );
  });
});
