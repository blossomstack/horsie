
import { SubAgentView } from './subAgentView';
/**
 * The session&#x27;s agent roster changed (a subagent spawned, finished, or failed).
 */
export interface AgentTreeEvent {
  agents: SubAgentView[];
}