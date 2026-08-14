import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SessionTimeline } from "./SessionTimeline";
import type { Timeline } from "../lib/timeline";

// vitest runs without `globals`, so testing-library's auto-cleanup never fires.
afterEach(cleanup);

const TIMELINE: Timeline = {
  width: 400,
  gaps: [{ x: 120, elapsedMs: 3_600_000 }],
  ticks: [{ x: 0, label: "09:12" }],
  lanes: [
    {
      agentId: "main",
      kind: "main",
      label: "main agent",
      status: "idle",
      depth: 0,
      placed: true,
      bars: [
        { key: "b1", kind: "user", x: 0, width: 20, entryId: "m1", title: "hi", detail: "09:12" },
        { key: "b2", kind: "tool", x: 24, width: 80, entryId: "m2", title: "Bash", detail: "12.4s" },
      ],
    },
    {
      agentId: "s1",
      kind: "subagent",
      label: "Explore",
      status: "completed",
      depth: 0,
      placed: true,
      bars: [],
      span: { x: 30, width: 60, open: false },
      anchor: { x: 30, parentAgentId: "main" },
    },
    {
      agentId: "f1",
      kind: "fork",
      label: "try postgres",
      status: "idle",
      depth: 0,
      placed: true,
      bars: [],
      span: { x: 90, width: 310, open: true },
      anchor: { x: 90, parentAgentId: "main" },
    },
    {
      agentId: "old",
      kind: "subagent",
      label: "ancient",
      status: "completed",
      depth: 0,
      placed: false,
      bars: [],
    },
  ],
};

const view = (timeline: Timeline, handlers: Partial<{ entry: (id: string) => void; agent: (id: string) => void }> = {}) =>
  render(
    <SessionTimeline
      timeline={timeline}
      onSelectEntry={handlers.entry ?? vi.fn()}
      onSelectAgent={handlers.agent ?? vi.fn()}
    />,
  );

describe("SessionTimeline", () => {
  it("draws a lane per agent", () => {
    view(TIMELINE);
    expect(screen.getByTestId("timeline-lane-main")).toBeTruthy();
    expect(screen.getByTestId("timeline-lane-s1")).toBeTruthy();
    expect(screen.getByTestId("timeline-lane-f1")).toBeTruthy();
  });

  it("hands a clicked bar's entry back", () => {
    const entry = vi.fn();
    view(TIMELINE, { entry });
    fireEvent.click(screen.getByTestId("timeline-bar-m2"));
    expect(entry).toHaveBeenCalledWith("m2");
  });

  it("hands a clicked lane's agent back", () => {
    const agent = vi.fn();
    view(TIMELINE, { agent });
    fireEvent.click(screen.getByTestId("timeline-span-s1"));
    expect(agent).toHaveBeenCalledWith("s1");
  });

  it("separates forks from subagents", () => {
    view(TIMELINE);
    expect(screen.getByText("forked conversations")).toBeTruthy();
  });

  it("says what a collapsed gap swallowed", () => {
    view(TIMELINE);
    expect(screen.getAllByTestId("timeline-gap")[0].textContent).toContain("1h");
  });

  it("keeps an unplaced agent visible, outside the axis", () => {
    view(TIMELINE);
    const lane = screen.getByTestId("timeline-lane-old");
    expect(lane.getAttribute("data-placed")).toBe("false");
    // No span: there was nothing to place it by, and a span at zero would be a
    // claim about when it ran.
    expect(screen.queryByTestId("timeline-span-old")).toBeNull();
  });

  it("puts the real duration in every bar's tooltip", () => {
    view(TIMELINE);
    expect(screen.getByTestId("timeline-bar-m2").getAttribute("title")).toBe("Bash · 12.4s");
  });

  it("says so when there is nothing to draw", () => {
    view({ lanes: [], gaps: [], ticks: [], width: 0 });
    expect(screen.getByTestId("timeline-empty")).toBeTruthy();
  });

  it("says so when the roster arrived but the transcript has not", () => {
    view({
      lanes: [{ agentId: "main", kind: "main", label: "main agent", status: "idle", depth: 0, placed: true, bars: [] }],
      gaps: [],
      ticks: [],
      width: 0,
    });
    expect(screen.getByTestId("timeline-empty")).toBeTruthy();
  });
});

describe("SessionTimeline anchors", () => {
  it("draws a connector back to the lane an agent came from", () => {
    // The spec's "arrow pointing to the timeline". `anchor` was computed in the
    // model and never rendered — invisible in every unit test, obvious in a
    // screenshot.
    view(TIMELINE);
    expect(screen.getByTestId("timeline-anchor-s1")).toBeTruthy();
    expect(screen.getByTestId("timeline-anchor-f1")).toBeTruthy();
  });

  it("draws no connector for an agent that could not be placed", () => {
    view(TIMELINE);
    expect(screen.queryByTestId("timeline-anchor-old")).toBeNull();
  });

  it("puts the connector at the moment the agent branched off", () => {
    view(TIMELINE);
    const anchor = screen.getByTestId("timeline-anchor-s1");
    expect(anchor.getAttribute("style")).toContain("left: 30px");
  });
});
