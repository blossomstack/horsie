
import { ProvisionResult } from './provisionResult';
export interface ProvisionAgentResponse {
  callId: string;
  /**
   * Absolute root of this agent's tree. The agent reads its skills' sibling
   * files through this, so it has to be a path that means something inside
   * the runtime — which only the runtime knows.
   */
  root: string;
  result: ProvisionResult;
}