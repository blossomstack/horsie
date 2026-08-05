
import { SubAgentView } from './subAgentView';
/**
 * The session&#39;s agent roster changed (a subagent spawned, finished, or failed).
 */
export interface AgentTreeEvent {
  agents: SubAgentView[];
}