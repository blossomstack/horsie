
/**
 * Derived, compact diagnostics from durable counters. Durations are provider
 * generation and completed tool execution spans, not end-to-end turn latency.
 */
export interface EfficiencyDiagnostic {
  averageInputTokensPerProviderCall: number;
  averageOutputTokensPerProviderCall: number;
  averageProviderGenerationMs: number;
  averageToolExecutionMs: number;
  cacheReadPercent?: number;
  toolFailurePercent?: number;
  outputRetentionPercent?: number;
  findings: string[];
}