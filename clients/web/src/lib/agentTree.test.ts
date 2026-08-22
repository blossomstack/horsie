import { describe, expect, it } from "vitest";
import { layoutAgentTree } from "./agentTree";
import type { SubAgentView, SubSessionView } from "../api/types";

function agent(
  id: string,
  parent?: string,
  depth = parent ? 1 : 0,
  spawnedAtMs = 1,
): SubAgentView {
  return {
    id,
    parent,
    label: undefined,
    depth,
    agentType: undefined,
    status: "completed",
    error: undefined,
    spawnedAtMs,
    endedAtMs: spawnedAtMs + 1000,
  };
}

function subSession(
  id: string,
  parent?: string,
  title?: string,
  createdAtMs = 1,
): SubSessionView {
  return {
    id,
    parent,
    title,
    status: "idle",
    createdAtMs,
    lastActivityMs: createdAtMs + 1000,
  };
}

/** The main agent as the roster carries it: nothing spawned it. */
const main = agent("main", undefined, 0, 0);

function lanes(nodes: { id: string; lane: number }[]): Record<string, number> {
  return Object.fromEntries(nodes.map((n) => [n.id, n.lane]));
}

describe("layoutAgentTree", () => {
  it("has nothing to draw for a roster that has not arrived", () => {
    const tree = layoutAgentTree([]);
    expect(tree.nodes).toEqual([]);
    expect(tree.rows).toBe(0);
  });

  it("roots on the agent nothing spawned", () => {
    const tree = layoutAgentTree([main, agent("a")]);
    expect(tree.nodes[0]).toMatchObject({ id: "main", kind: "main", depth: 0 });
    expect(tree.edges).toEqual([{ from: "main", to: "a" }]);
  });

  /* The schema says an absent parent means "rooted on the primary agent". An
   * agent that names the main agent outright means the same thing, and the two
   * conventions have to land in one bucket or the picture is a forest. */
  it("treats a parent of the main agent the same as no parent at all", () => {
    const named = layoutAgentTree([main, agent("a", "main")]);
    const absent = layoutAgentTree([main, agent("a")]);
    expect(named.nodes.map((n) => [n.id, n.depth])).toEqual(
      absent.nodes.map((n) => [n.id, n.depth]),
    );
  });

  it("nests to any depth", () => {
    const tree = layoutAgentTree([main, agent("a"), agent("b", "a", 2), agent("c", "b", 3)]);
    expect(tree.nodes.map((n) => [n.id, n.depth])).toEqual([
      ["main", 0],
      ["a", 1],
      ["b", 2],
      ["c", 3],
    ]);
    expect(tree.depth).toBe(4);
  });

  it("orders siblings oldest first, so a relabel never moves a node", () => {
    const tree = layoutAgentTree([main, agent("b", undefined, 1, 20), agent("a", undefined, 1, 10)]);
    expect(tree.nodes.map((n) => n.id)).toEqual(["main", "a", "b"]);
  });

  it("gives each leaf its own row and centres a parent over its children", () => {
    const tree = layoutAgentTree([
      main,
      agent("a", undefined, 1, 1),
      agent("b", undefined, 1, 2),
      agent("c", undefined, 1, 3),
    ]);
    expect(lanes(tree.nodes)).toEqual({ main: 1, a: 0, b: 1, c: 2 });
    expect(tree.rows).toBe(3);
  });

  it("centres a parent between an even number of children", () => {
    const tree = layoutAgentTree([main, agent("a", undefined, 1, 1), agent("b", undefined, 1, 2)]);
    expect(lanes(tree.nodes).main).toBe(0.5);
  });

  describe("folding", () => {
    const roster = [main, agent("a"), agent("b", "a", 2), agent("c", "b", 3)];

    it("drops a folded agent's descendants from the picture", () => {
      const tree = layoutAgentTree(roster, [], ["a"]);
      expect(tree.nodes.map((n) => n.id)).toEqual(["main", "a"]);
      expect(tree.edges).toEqual([{ from: "main", to: "a" }]);
      expect(tree.hidden).toBe(2);
    });

    it("reports what a fold stands for, so the node can say how much is hidden", () => {
      const tree = layoutAgentTree(roster, [], ["a"]);
      expect(tree.nodes[1]).toMatchObject({ collapsed: true, children: 1, descendants: 2 });
    });

    /* The count is a fact about the roster. Folding must not change it, or a
     * node would claim to hide less the deeper you folded. */
    it("counts descendants the same whether or not they are drawn", () => {
      const open = layoutAgentTree(roster);
      const shut = layoutAgentTree(roster, [], ["a"]);
      const descendants = (t: typeof open, id: string) =>
        t.nodes.find((n) => n.id === id)?.descendants;
      expect(descendants(shut, "a")).toBe(descendants(open, "a"));
    });

    it("closes the rows a fold freed, rather than leaving a gap", () => {
      const tree = layoutAgentTree(
        [main, agent("a", undefined, 1, 1), agent("x", "a", 2), agent("b", undefined, 1, 2)],
        [],
        ["a"],
      );
      expect(lanes(tree.nodes)).toEqual({ main: 0.5, a: 0, b: 1 });
      expect(tree.rows).toBe(2);
    });

    it("ignores a fold on an agent that has nothing under it", () => {
      const tree = layoutAgentTree([main, agent("a")], [], ["a"]);
      expect(tree.nodes[1].collapsed).toBe(false);
    });
  });

  describe("rosters that are not trees", () => {
    /* Journal-derived data. An agent nobody can find is worse than one drawn in
     * the wrong place, so neither case may drop a row. */
    it("hangs an agent whose parent is missing off the main agent", () => {
      const tree = layoutAgentTree([main, agent("orphan", "gone", 3)]);
      expect(tree.nodes.map((n) => [n.id, n.depth])).toEqual([
        ["main", 0],
        ["orphan", 1],
      ]);
    });

    it("draws a cycle once instead of recurring forever", () => {
      const tree = layoutAgentTree([
        main,
        agent("a", "b", 1),
        agent("b", "a", 2),
      ]);
      expect(tree.nodes.map((n) => n.id).sort()).toEqual(["a", "b", "main"]);
      expect(tree.nodes.filter((n) => n.id === "a")).toHaveLength(1);
    });

    it("does not let an agent be its own parent", () => {
      const tree = layoutAgentTree([main, agent("a", "a")]);
      expect(tree.nodes.map((n) => [n.id, n.depth])).toEqual([
        ["main", 0],
        ["a", 1],
      ]);
    });
  });

  describe("sub sessions", () => {
    /* They are not agents the session spawned, but they are the same lineage,
       and the graph is the one place a person can reach one now that the rail
       lists sessions only. */
    it("draws a sub session hanging off the session it branched from", () => {
      const tree = layoutAgentTree([main], [subSession("s", undefined, "the other migration")]);
      expect(tree.nodes.map((n) => [n.id, n.kind, n.depth])).toEqual([
        ["main", "main", 0],
        ["s", "sub_session", 1],
      ]);
      expect(tree.edges).toEqual([{ from: "main", to: "s" }]);
      expect(tree.nodes[1].label).toBe("the other migration");
    });

    it("nests a sub session of a sub session under the one it came from", () => {
      const tree = layoutAgentTree(
        [main],
        [subSession("a", undefined, "first", 1), subSession("b", "a", "second", 2)],
      );
      expect(tree.nodes.map((n) => [n.id, n.depth])).toEqual([
        ["main", 0],
        ["a", 1],
        ["b", 2],
      ]);
    });

    /* The reason both rosters have to be laid out together. A subagent spawned
       by a sub session names it as its parent; with only the agents in hand
       there was nothing to hang it on, so it came out rooted on the main agent
       — beside the sub session that spawned it rather than under it. */
    it("hangs a subagent spawned by a sub session under that sub session", () => {
      const tree = layoutAgentTree([main, agent("sub", "s", 2)], [subSession("s")]);
      expect(tree.nodes.map((n) => [n.id, n.depth])).toEqual([
        ["main", 0],
        ["s", 1],
        ["sub", 2],
      ]);
      expect(tree.edges).toEqual([
        { from: "main", to: "s" },
        { from: "s", to: "sub" },
      ]);
    });

    it("says what an unnamed sub session is rather than showing its id", () => {
      const tree = layoutAgentTree([main], [subSession("s")]);
      expect(tree.nodes[1].label).toBe("untitled sub session");
    });

    /* One lineage, one ordering: a sub session branched before a subagent was
       spawned is drawn above it. */
    it("orders sub sessions and subagents together, oldest first", () => {
      const tree = layoutAgentTree(
        [main, agent("late", undefined, 1, 30)],
        [subSession("early", undefined, "early", 10)],
      );
      expect(tree.nodes.map((n) => n.id)).toEqual(["main", "early", "late"]);
    });

    it("folds a sub session's descendants like any other node", () => {
      const tree = layoutAgentTree([main, agent("sub", "s", 2)], [subSession("s")], ["s"]);
      expect(tree.nodes.map((n) => n.id)).toEqual(["main", "s"]);
      expect(tree.nodes[1]).toMatchObject({ collapsed: true, descendants: 1 });
      expect(tree.hidden).toBe(1);
    });
  });

  it("names an agent by its label, else its preset, else what it is", () => {
    const labelled = { ...agent("a"), label: "review the diff" };
    const preset = { ...agent("b"), agentType: "code-reviewer" };
    const tree = layoutAgentTree([main, labelled, preset, agent("c")]);
    expect(tree.nodes.map((n) => n.label)).toEqual([
      "main agent",
      "review the diff",
      "code-reviewer",
      "subagent",
    ]);
  });
});
