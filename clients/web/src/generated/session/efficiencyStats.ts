
/**
 * Cumulative counters that explain where an agent run spent work and context.
 */
export interface EfficiencyStats {
  providerCalls: number;
  providerGenerationMs: number;
  maxProviderGenerationMs: number;
  toolCalls: number;
  resultToolCalls: number;
  toolExecutionMs: number;
  maxToolExecutionMs: number;
  failedToolCalls: number;
  /**
   * Bytes retained in the transcript after output guards.
   */
  toolResultBytes: number;
  /**
   * Bytes produced before output guards.
   */
  originalToolResultBytes: number;
  /**
   * Bytes omitted from model context by output guards.
   */
  truncatedToolResultBytes: number;
  /**
   * Complete bytes retained in runtime spill files.
   */
  spilledToolResultBytes: number;
  completedRuns: number;
  abortedRuns: number;
  compactions: number;
}