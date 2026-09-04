
/**
 * Cumulative counters that explain where an agent run spent work and context.
 */
export interface EfficiencyStats {
  providerCalls: number;
  toolCalls: number;
  failedToolCalls: number;
  toolResultBytes: number;
  completedRuns: number;
  abortedRuns: number;
  compactions: number;
}