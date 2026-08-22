
/**
 * One agent run, as the index holds it: where to find it, and enough to
 * triage it by.
 *
 * An address plus an outcome, deliberately. Everything else about a run —
 * what it was asked, what it did, what it cost — is on `sessions.get` and
 * `sessions.read` once you hold these two ids, and answering it here would
 * make a listing of a hundred runs cost what reading one does.
 */
export interface AgentRunView {
  /**
   * The session hosting this agent. Pass it to `sessions.get` or
   * `sessions.read`.
   */
  sessionId: string;
  /**
   * "main", or the agent's uuid. The `aid` that `sessions.read` takes.
   */
  agentId: string;
  /**
   * The saved preset this agent ran under; absent when its settings were
   * supplied inline.
   */
  preset?: string;
  /**
   * "provisioning" | "running" | "idle" | "awaiting_input" | "completed" |
   * "failed" | "cancelled". The roster's vocabulary.
   */
  status: string;
  /**
   * Unix epoch ms. Zero for a session's main agent, which nothing spawned.
   */
  startedAtMs: number;
  /**
   * Unix epoch ms, absent while the run is still going — and absent for
   * good on a main agent, which is a session and never ends.
   */
  endedAtMs?: number;
}