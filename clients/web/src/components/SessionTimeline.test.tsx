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
      hasChildren: true,
      detail: "idle",
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
      hasChildren: true,
      detail: "idle",
      bars: [],
      span: { x: 30, width: 60, open: false },
      anchor: { x: 30, parentAgentId: "main" },
    },
    {
      agentId: "f1",
      kind: "sub_session",
      label: "try postgres",
      status: "idle",
      depth: 0,
      placed: true,
      hasChildren: true,
      detail: "idle",
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
      hasChildren: false,
      detail: "completed",
      bars: [],
    },
  ],
};

const view = (
  timeline: Timeline,
  handlers: Partial<{
    entry: (id: string) => void;
    agent: (id: string) => void;
    expand: (id: string) => void;
    expanded: string[];
    collapse: (id: string) => void;
    collapsed: string[];
    open: (id: string) => void;
  }> = {},
) =>
  render(
    <SessionTimeline
      timeline={timeline}
      expanded={handlers.expanded ?? []}
      collapsed={handlers.collapsed ?? []}
      onToggleCollapse={handlers.collapse ?? vi.fn()}
      onToggleExpand={handlers.expand ?? vi.fn()}
      onSelectEntry={handlers.entry ?? vi.fn()}
      onSelectAgent={handlers.agent ?? vi.fn()}
      onOpenAgent={handlers.open ?? vi.fn()}
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
    fireEvent.click(screen.getByTestId("timeline-bar-b2"));
    expect(entry).toHaveBeenCalledWith("m2");
  });

  /** The name shows what the agent *is*, beside the picture. It used to
   *  navigate, which answered "what is this lane?" by closing the timeline
   *  that raised the question. */
  it("shows an agent from its name in the sidebar rather than leaving", () => {
    const agent = vi.fn();
    const open = vi.fn();
    view(TIMELINE, { agent, open });
    fireEvent.click(screen.getByTestId("timeline-select-s1"));
    expect(agent).toHaveBeenCalledWith("s1");
    expect(open).not.toHaveBeenCalled();
  });

  /** A key only where there is somewhere to go.
   *
   * The session's own run is the lane at the top, and its page is the run
   * view — which is what a step's page needs to get back to. A run an agent
   * *invoked* is a lane further down and has no page at all: nothing renders
   * one on its own, and the key used to go to the inviting session's
   * transcript, which is neither the run nor what the key said it opened. */
  it("offers no jump key on a run that has no page of its own", () => {
    const runLane = (agentId: string, depth: number) => ({
      agentId,
      kind: "run" as const,
      label: "nightly-audit",
      status: "completed",
      depth,
      placed: true,
      hasChildren: true,
      detail: "completed",
      bars: [],
      span: { x: 10, width: 40, open: false },
    });
    view({
      ...TIMELINE,
      lanes: [runLane("run:own", 0), runLane("run:invoked", 1)],
    });
    expect(screen.queryByTestId("timeline-open-run:own")).toBeTruthy();
    expect(screen.queryByTestId("timeline-open-run:invoked")).toBeNull();
  });

  it("leaves for an agent's transcript from the jump key beside its name", () => {
    const agent = vi.fn();
    const open = vi.fn();
    view(TIMELINE, { agent, open });
    fireEvent.click(screen.getByTestId("timeline-open-s1"));
    expect(open).toHaveBeenCalledWith("s1");
    expect(agent).not.toHaveBeenCalled();
  });

  it("expands a lane from its span, and from the chevron beside its name", () => {
    const expand = vi.fn();
    view(TIMELINE, { expand });
    fireEvent.click(screen.getByTestId("timeline-span-s1"));
    fireEvent.click(screen.getByTestId("timeline-span-s1"));
    expect(expand).toHaveBeenNthCalledWith(1, "s1");
    expect(expand).toHaveBeenNthCalledWith(2, "s1");
  });

  it("draws a lane's own bars once it is expanded", () => {
    const withOwn: Timeline = {
      ...TIMELINE,
      lanes: TIMELINE.lanes.map((l) =>
        l.agentId === "s1"
          ? {
              ...l,
              bars: [
                { key: "s1:x", kind: "tool", x: 32, width: 40, entryId: "sm1", title: "grep", detail: "1.2s" },
              ],
            }
          : l,
      ),
    };
    view(withOwn, { expanded: ["s1"] });
    expect(screen.getByTestId("timeline-bar-s1:x")).toBeTruthy();
    expect(screen.getByTestId("timeline-lane-s1").getAttribute("data-expanded")).toBe("true");
  });

  /* Grouping says it, so nothing has to be drawn to say it. The rule could
     only ever be drawn once — at the first sub session in a flat list of lanes
     — so under a tree where each agent had both kinds of child it landed in
     the middle of one agent's children and said nothing true about the rest.
     `bySibling` in `agentTree` is what orders them now. */
  it("spends no row on a rule between the two kinds of child", () => {
    view(TIMELINE);
    expect(screen.queryByText("sub sessions")).toBeNull();
    // Every lane it was handed is still drawn, in the order it was handed them.
    expect(
      screen
        .getAllByTestId(/^timeline-lane-/)
        .map((el) => el.getAttribute("data-testid")),
    ).toEqual(TIMELINE.lanes.map((l) => `timeline-lane-${l.agentId}`));
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
    expect(screen.getByTestId("timeline-bar-b2").getAttribute("title")).toBe("Bash · 12.4s");
  });

  it("says so when there is nothing to draw", () => {
    view({ lanes: [], gaps: [], ticks: [], width: 0 });
    expect(screen.getByTestId("timeline-empty")).toBeTruthy();
  });

  it("says so when the roster arrived but the transcript has not", () => {
    view({
      lanes: [{ agentId: "main", kind: "main", label: "main agent", status: "idle", depth: 0, placed: true, hasChildren: false, detail: "idle", bars: [] }],
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

describe("disclosure and the hover card", () => {
  const NESTED: Timeline = {
    ...TIMELINE,
    lanes: [
      { ...TIMELINE.lanes[0], hasChildren: true },
      { ...TIMELINE.lanes[1], hasChildren: true, depth: 0 },
      {
        agentId: "s2",
        kind: "subagent",
        label: "a very long subagent name that the sidebar cannot possibly fit",
        status: "completed",
        depth: 1,
        placed: true,
        hasChildren: false,
        detail: "completed · 3.4s · started 09:13",
        bars: [],
        span: { x: 40, width: 20, open: false },
        anchor: { x: 40, parentAgentId: "s1" },
      },
      TIMELINE.lanes[2],
      TIMELINE.lanes[3],
    ],
  };

  it("offers a chevron only where something hangs off the lane", () => {
    view(NESTED);
    expect(screen.getByTestId("timeline-collapse-s1")).toBeTruthy();
    expect(screen.queryByTestId("timeline-collapse-s2")).toBeNull();
  });

  it("carries the whole name in the hover card, which the sidebar cannot fit", () => {
    view(NESTED);
    const card = screen.getByTestId("timeline-card-s2");
    expect(card.textContent).toContain(
      "a very long subagent name that the sidebar cannot possibly fit",
    );
    expect(card.textContent).toContain("completed · 3.4s · started 09:13");
  });
});
