
/**
 * A subagent starting. `agent_id` names *which* subagent; `agent_type` is the
 * matcher domain, the way `source` is for `SessionStart`. Both, because until
 * #105's Phase 2 gives horsie an agent-type concept every subagent reports the
 * same type, and the id is then the only thing telling two apart.
 */
export interface SubagentStartInput {
  agentId: string;
  agentType: string;
}