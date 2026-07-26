
import { Usage } from '../agent';
import { UsageView } from './usageView';
/**
 * One agent&#x27;s own usage plus its context-size snapshot, labeled with the
 */
export interface AgentUsageView {
  model: string;
  usageTotal: UsageView;
  /**
   * The most recently completed run&#x27;s own usage — a per-run cost figure.
   */
  lastTurnUsage?: Usage;
  /**
   * The most recently completed run&#x27;s last provider call&#x27;s prompt size
   */
  contextTokens: number;
  /**
   * The model&#x27;s configured context window, when known.
   */
  contextWindow?: number;
}