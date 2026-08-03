
import { AgentTokenView } from './agentTokenView';
export interface AgentTokenCreated {
  /**
   * Shown once and never again.
   */
  token: string;
  view: AgentTokenView;
}