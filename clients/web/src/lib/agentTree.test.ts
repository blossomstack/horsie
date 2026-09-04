import { describe, expect, it } from "vitest";
import { layoutAgentTree, runNodeId, runStatus, stepRuns } from "./agentTree";
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
    title: undefined,
    kind: "subagent",
    stats: {
      usage: { inputTokens: 0, outputTokens: 0 },
      subtreeUsage: { inputTokens: 0, outputTokens: 0 },
      contextTokens: 0,
      efficiency: {
              providerCalls: 0,
              toolCalls: 0,
              failedToolCalls: 0,
              toolResultBytes: 0,
              completedRuns: 0,
              abortedRuns: 0,
              compactions: 0,
            },
    },
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
  title = "a branch",
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

/** The main agent as the roster carries it: nothing spawned it, and it says
 *  so — `kind` is what tells a run (which has no main agent) from a session
 *  that merely invoked one. */
const main: SubAgentView = { ...agent("main", undefined, 0, 0), kind: "main" };

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

    /* A sub session is named at the branch, by whoever branched it — the tool
       takes a title and `/fork` derives one — so there is no such thing as an
       unnamed one and nothing has to invent a name for it. */
    it("draws a sub session under the title it was branched with", () => {
      const tree = layoutAgentTree([main], [subSession("s", undefined, "the other migration")]);
      expect(tree.nodes[1].label).toBe("the other migration");
    });

    /* Two kinds of thing hang off one agent, and grouping them is what says so
       — a subagent is work inside a turn, a sub session is another session.
       Interleaved by spawn time the reader had to sort them, and the timeline
       was spending a labelled row on a rule that only said it once. */
    it("draws the delegated work first, then the sessions branched off", () => {
      const tree = layoutAgentTree(
        [main, agent("late", undefined, 1, 30)],
        [subSession("early", undefined, "early", 10)],
      );
      expect(tree.nodes.map((n) => n.id)).toEqual(["main", "late", "early"]);
    });

    it("keeps each group oldest first inside itself", () => {
      const tree = layoutAgentTree(
        [main, agent("agent-late", undefined, 1, 40), agent("agent-early", undefined, 1, 20)],
        [
          subSession("sub-late", undefined, "later", 50),
          subSession("sub-early", undefined, "earlier", 10),
        ],
      );
      expect(tree.nodes.map((n) => n.id)).toEqual([
        "main",
        "agent-early",
        "agent-late",
        "sub-early",
        "sub-late",
      ]);
    });

    /* Under every agent, not just the root: the timeline's old divider was one
       rule drawn at the first sub session in a flat list of lanes, so with two
       agents that each had both it landed inside one of them. */
    it("groups the children of every agent, not only the root's", () => {
      const tree = layoutAgentTree(
        [main, agent("branch", undefined, 1, 1), agent("kid", "branch", 2, 40)],
        [subSession("kid-sub", "branch", "under branch", 20)],
      );
      expect(tree.nodes.map((n) => n.id)).toEqual(["main", "branch", "kid", "kid-sub"]);
    });

    it("folds a sub session's descendants like any other node", () => {
      const tree = layoutAgentTree([main, agent("sub", "s", 2)], [subSession("s")], ["s"]);
      expect(tree.nodes.map((n) => n.id)).toEqual(["main", "s"]);
      expect(tree.nodes[1]).toMatchObject({ collapsed: true, descendants: 1 });
      expect(tree.hidden).toBe(1);
    });
  });

  /** A workflow run has no main agent — it *is* its steps, and every one of
   *  them reaches the roster parentless, because the definition chose it. Both
   *  pictures rooted on whichever step came first and hung the rest off it: a
   *  three-step run drew as one step labelled "main session" that had somehow
   *  spawned the other two, in no particular order. */
  describe("workflow runs", () => {
    /** One execution, as the roster reports it: named for the step it ran and
     *  carrying the run it belongs to. */
    const step = (
      id: string,
      at: number,
      status = "completed",
      run = "run-1",
      workflow = "nightly-audit",
      parent?: string,
    ): SubAgentView => ({
      ...agent(id, parent, 0, at),
      kind: "step",
      title: id,
      status,
      run,
      workflow,
    });
    const RUN_ROOT = runNodeId("run-1");

    it("roots on the run, and chains its steps in the order they ran", () => {
      const tree = layoutAgentTree(
        [step("report", 30), step("gather", 10), step("review", 20)],
        [],
        [],
        "nightly-audit",
      );
      expect(tree.nodes.map((n) => [n.id, n.kind, n.depth])).toEqual([
        [RUN_ROOT, "run", 0],
        ["gather", "step", 1],
        ["review", "step", 2],
        ["report", "step", 3],
      ]);
      // One edge per transition, and none of them from the run to a later step:
      // a run is a sequence, and a fan of edges out of the root is not one.
      expect(tree.edges).toEqual([
        { from: RUN_ROOT, to: "gather" },
        { from: "gather", to: "review" },
        { from: "review", to: "report" },
      ]);
      // The run node is named for the workflow it is a run of.
      expect(tree.nodes[0].label).toBe("nightly-audit");
    });

    /* A step delegates like any other agent, and what it delegated is its
       own — drawn under it, above the step the run went to next. */
    it("hangs a step's subagents off that step, before the step that followed", () => {
      const tree = layoutAgentTree(
        [step("gather", 10), step("review", 20), agent("helper", "gather", 1, 12)],
        [],
        [],
        "nightly-audit",
      );
      expect(tree.nodes.map((n) => n.id)).toEqual([RUN_ROOT, "gather", "helper", "review"]);
      expect(tree.edges).toEqual([
        { from: RUN_ROOT, to: "gather" },
        { from: "gather", to: "helper" },
        { from: "gather", to: "review" },
      ]);
    });

    it("folds the whole run away from its root", () => {
      const tree = layoutAgentTree(
        [step("gather", 10), step("review", 20)],
        [],
        [RUN_ROOT],
        "nightly-audit",
      );
      expect(tree.nodes.map((n) => n.id)).toEqual([RUN_ROOT]);
      expect(tree.nodes[0]).toMatchObject({ collapsed: true, descendants: 2 });
    });

    /** Steps in the roster do not make the session a run.
     *
     * An agent that calls `invoke_workflow` starts a run *inside* an ordinary
     * session, and the roster lists those executions too — the server says so:
     * "every run's executions, the session's own and any invoked one's".
     * Rooted on the steps, such a session came out with its own main agent
     * swept inside a run it merely started. It has a main agent, so it is not
     * a run; the run hangs off it like any other work it set going. */
    it("hangs a run an agent invoked off that agent, without rerooting the session", () => {
      const tree = layoutAgentTree([main, step("gather", 10), step("review", 20)]);
      expect(tree.nodes.map((n) => [n.id, n.kind, n.depth])).toEqual([
        ["main", "main", 0],
        [RUN_ROOT, "run", 1],
        ["gather", "step", 2],
        ["review", "step", 3],
      ]);
    });

    /** The shape the server used to flatten: a session invokes a workflow,
     *  and one of that run's steps spawns a subagent of its own. The step's
     *  child reached the client parentless and drew on the main agent, three
     *  ranks away from the step that actually spawned it. */
    it("hangs the subagent of an invoked run's step off that step", () => {
      const tree = layoutAgentTree([
        main,
        step("plan", 10),
        step("code", 20),
        agent("toolchain", "code", 1, 22),
      ]);
      expect(tree.nodes.map((n) => [n.id, n.depth])).toEqual([
        ["main", 0],
        [RUN_ROOT, 1],
        ["plan", 2],
        ["code", 3],
        ["toolchain", 4],
      ]);
      expect(tree.edges).toContainEqual({ from: "code", to: "toolchain" });
      expect(tree.edges).not.toContainEqual({ from: "main", to: "toolchain" });
    });

    /** A session can host several at once — its own and one per invocation,
     *  or two invocations from different agents. Flattened into one list they
     *  drew as a single impossible run whose steps interleaved. */
    it("draws one run per run, not one run per session", () => {
      const tree = layoutAgentTree([
        main,
        step("a1", 10, "completed", "run-1", "audit"),
        step("b1", 20, "completed", "run-2", "release"),
        step("a2", 30, "completed", "run-1", "audit"),
      ]);
      const runs = tree.nodes.filter((n) => n.kind === "run");
      expect(runs.map((n) => [n.id, n.label])).toEqual([
        [runNodeId("run-1"), "audit"],
        [runNodeId("run-2"), "release"],
      ]);
      // Each run's own executions chain under it, and never across runs.
      expect(tree.edges).toEqual([
        { from: "main", to: runNodeId("run-1") },
        { from: runNodeId("run-1"), to: "a1" },
        { from: "a1", to: "a2" },
        { from: "main", to: runNodeId("run-2") },
        { from: runNodeId("run-2"), to: "b1" },
      ]);
    });

    it("groups a run the roster hangs off a subagent under that subagent", () => {
      const tree = layoutAgentTree([
        main,
        agent("worker", undefined, 1, 5),
        step("gather", 10, "completed", "run-1", "audit", "worker"),
      ]);
      expect(tree.nodes.map((n) => [n.id, n.depth])).toEqual([
        ["main", 0],
        ["worker", 1],
        [runNodeId("run-1"), 2],
        ["gather", 3],
      ]);
    });

    it("orders the run log by when each execution began", () => {
      expect(stepRuns([step("c", 30), step("a", 10), step("b", 20)]).map((s) => s.id)).toEqual([
        "a",
        "b",
        "c",
      ]);
    });

    /* A run's status is folded from its steps, because the session's own is in
       a different vocabulary — a session is `Finished`, an agent `completed`. */
    it("folds the run's status from its steps, a fault outranking a finish", () => {
      expect(runStatus([step("a", 1), step("b", 2)])).toBe("completed");
      expect(runStatus([step("a", 1), step("b", 2, "running")])).toBe("running");
      // The last step landing does not undo the middle one failing.
      expect(runStatus([step("a", 1, "failed"), step("b", 2)])).toBe("failed");
      expect(runStatus([])).toBe("idle");
    });
  });

  it("names an agent by its title, else its preset, else what it is", () => {
    const titled = { ...agent("a"), title: "review the diff" };
    const preset = { ...agent("b"), agentType: "code-reviewer" };
    const tree = layoutAgentTree([main, titled, preset, agent("c")]);
    expect(tree.nodes.map((n) => n.label)).toEqual([
      // The main agent has no title in this roster, so it falls back to what
      // it is. Given one, that title is the session's name.
      "main agent",
      "review the diff",
      "code-reviewer",
      "subagent",
    ]);
  });

  /* The main agent carries the session's title, because naming the session
     *is* naming its main agent. It used to read "main agent" — the one node in
     the picture that said what it was instead of what it was doing. */
  it("draws the main agent under the session's own title", () => {
    const tree = layoutAgentTree([{ ...main, title: "port the journal" }]);
    expect(tree.nodes[0].label).toBe("port the journal");
  });
});
