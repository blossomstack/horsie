import { isRunNode } from "./agentTree";

/**
 * Whether an agent's page *is* the session's page.
 *
 * Pressing "open" on an agent in a structural view goes to that agent's own
 * page — except for the two things that have no page of their own: the
 * session's main agent, whose page is the session's, and a run node, which is
 * not an agent at all.
 *
 * A run has no main agent. Its `agents` list is one entry per step execution,
 * and the first of those is rooted at depth 0 — indistinguishable, by shape,
 * from a session's own agent. Reading it as one sent every "open the start
 * step" to the run's graph, which is the page the button was pressed *on*.
 * So `isRun` is asked first, and on a run nothing is the session.
 */
export function isSessionsOwnPage({
  agent,
  isRun,
  mainAgentId,
  mainAgentAlias,
}: {
  agent: string;
  /** The session is a workflow run. */
  isRun: boolean;
  /** The id of the session's own agent; absent on a run. */
  mainAgentId: string | undefined;
  /** The reserved id the API answers the session's own agent by. */
  mainAgentAlias: string;
}): boolean {
  if (isRunNode(agent)) return true;
  if (isRun) return false;
  return agent === mainAgentId || agent === mainAgentAlias;
}
