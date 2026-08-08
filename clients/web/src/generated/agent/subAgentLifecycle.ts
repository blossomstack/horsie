
/**
 * A subagent this agent spawned, and where it got to. Recorded on the
 * *parent*, because the parent is what a viewer is reading when it matters.
 */
export interface SubAgentLifecycle {
  id: string;
  label: string;
  status: string;
}