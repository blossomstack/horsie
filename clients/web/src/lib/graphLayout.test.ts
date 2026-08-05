import { describe, expect, it } from "vitest";
import { layoutGraph } from "./graphLayout";

const nodes = (...names: string[]) => names.map((step) => ({ step }));
const edge = (from: string, to: string) => ({ from, to });

const rankOf = (layout: ReturnType<typeof layoutGraph>, step: string) =>
  layout.nodes.find((n) => n.step === step)?.rank;

describe("layoutGraph", () => {
  it("puts the start step at rank 0 and each successor one deeper", () => {
    const l = layoutGraph(
      nodes("triage", "fix", "review"),
      [edge("triage", "fix"), edge("fix", "review")],
      "triage",
    );
    expect(rankOf(l, "triage")).toBe(0);
    expect(rankOf(l, "fix")).toBe(1);
    expect(rankOf(l, "review")).toBe(2);
    expect(l.depth).toBe(3);
    expect(l.breadth).toBe(1);
  });

  it("places a branch's two targets side by side in one rank", () => {
    const l = layoutGraph(
      nodes("triage", "fix", "file"),
      [edge("triage", "fix"), edge("triage", "file")],
      "triage",
    );
    expect(rankOf(l, "fix")).toBe(1);
    expect(rankOf(l, "file")).toBe(1);
    expect(l.breadth).toBe(2);
    // Order within the rank follows the definition's step order, so the graph
    // reads in the same sequence as the list beside it.
    const inRank = l.nodes.filter((n) => n.rank === 1).sort((a, b) => a.order - b.order);
    expect(inRank.map((n) => n.step)).toEqual(["fix", "file"]);
  });

  it("ranks a node by the shallowest step that reaches it", () => {
    // triage → review directly, and triage → fix → review. Review is 1, not 2.
    const l = layoutGraph(
      nodes("triage", "fix", "review"),
      [edge("triage", "review"), edge("triage", "fix"), edge("fix", "review")],
      "triage",
    );
    expect(rankOf(l, "review")).toBe(1);
  });

  it("marks an edge that returns to an earlier step as a back-edge", () => {
    const l = layoutGraph(
      nodes("fix", "review"),
      [edge("fix", "review"), edge("review", "fix")],
      "fix",
    );
    const forward = l.edges.find((e) => e.from === "fix");
    const loop = l.edges.find((e) => e.from === "review");
    expect(forward?.back).toBe(false);
    expect(loop?.back).toBe(true);
  });

  it("treats a self-loop as a back-edge", () => {
    const l = layoutGraph(nodes("poll"), [edge("poll", "poll")], "poll");
    expect(l.edges[0].back).toBe(true);
  });

  /// A step the start cannot reach is usually an authoring mistake. Hiding it
  /// would make the graph disagree with the step list beside it.
  it("keeps an unreachable step, flagged and ranked last", () => {
    const l = layoutGraph(
      nodes("triage", "fix", "orphan"),
      [edge("triage", "fix")],
      "triage",
    );
    const orphan = l.nodes.find((n) => n.step === "orphan");
    expect(orphan?.reachable).toBe(false);
    expect(orphan?.rank).toBe(2);
    expect(l.nodes.find((n) => n.step === "fix")?.reachable).toBe(true);
  });

  it("survives a start step that names nothing", () => {
    const l = layoutGraph(nodes("a", "b"), [edge("a", "b")], "nowhere");
    expect(l.nodes.every((n) => !n.reachable)).toBe(true);
    expect(l.nodes.every((n) => n.rank === 0)).toBe(true);
  });

  it("ignores an edge pointing at a step that does not exist", () => {
    const l = layoutGraph(nodes("a"), [edge("a", "ghost")], "a");
    expect(l.edges).toHaveLength(0);
    expect(l.nodes).toHaveLength(1);
  });

  it("terminates on a cycle rather than walking it forever", () => {
    const l = layoutGraph(
      nodes("a", "b", "c"),
      [edge("a", "b"), edge("b", "c"), edge("c", "a")],
      "a",
    );
    expect(l.nodes.map((n) => n.rank).sort()).toEqual([0, 1, 2]);
    expect(l.edges.filter((e) => e.back)).toHaveLength(1);
  });

  it("lays out an empty definition without dividing by zero", () => {
    const l = layoutGraph([], [], "start");
    expect(l).toEqual({ nodes: [], edges: [], breadth: 0, depth: 0 });
  });
});
