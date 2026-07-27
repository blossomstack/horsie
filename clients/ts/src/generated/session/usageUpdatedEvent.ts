
import { Usage } from '../agent';
/**
 * One provider call&#x27;s cost, journaled as the call returns. Lets a client keep
 */
export interface UsageUpdatedEvent {
  usage: Usage;
  contextTokens: number;
}