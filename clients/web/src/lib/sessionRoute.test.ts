import { describe, expect, it } from "vitest";
import { isSessionsOwnPage } from "./sessionRoute";
import { runNodeId } from "./agentTree";

const MAIN = "main";

describe("isSessionsOwnPage", () => {
  it("sends the session's own agent to the session's page", () => {
    expect(
      isSessionsOwnPage({
        agent: "agent-0",
        isRun: false,
        mainAgentId: "agent-0",
        mainAgentAlias: MAIN,
      }),
    ).toBe(true);
    expect(
      isSessionsOwnPage({
        agent: MAIN,
        isRun: false,
        mainAgentId: "agent-0",
        mainAgentAlias: MAIN,
      }),
    ).toBe(true);
  });

  it("sends a subagent to its own page", () => {
    expect(
      isSessionsOwnPage({
        agent: "agent-7",
        isRun: false,
        mainAgentId: "agent-0",
        mainAgentAlias: MAIN,
      }),
    ).toBe(false);
  });

  /**
   * The regression. A run's first step execution is rooted and at depth 0, so
   * whatever picks "the session's own agent" out of the roster picks it — and
   * every attempt to open the start step then landed back on the run's graph,
   * the page the button was pressed on. A run has no agent of its own, so on a
   * run this answers no for everything.
   */
  it("never calls a run's step the session, however it is rooted", () => {
    for (const agent of ["agent-0", MAIN, "agent-3"]) {
      expect(
        isSessionsOwnPage({
          agent,
          isRun: true,
          mainAgentId: undefined,
          mainAgentAlias: MAIN,
        }),
      ).toBe(false);
    }
  });

  /** A run an agent invoked is drawn as a node in the tree and is not a page:
   * nothing renders it on its own. */
  it("keeps a run node on the session's page", () => {
    expect(
      isSessionsOwnPage({
        agent: runNodeId("run-1"),
        isRun: false,
        mainAgentId: "agent-0",
        mainAgentAlias: MAIN,
      }),
    ).toBe(true);
  });
});
