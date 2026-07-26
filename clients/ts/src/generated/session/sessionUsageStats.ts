
import { AgentUsageView } from './agentUsageView';
import { UsageView } from './usageView';
/**
 * A session&#x27;s aggregated usage. `session_total` sums every agent this
 */
export interface SessionUsageStats {
  sessionTotal: UsageView;
  mainAgent: AgentUsageView;
}