
/**
 * Cumulative token usage across a session&#39;s completed turns. `u64` (not the
 */
export interface UsageView {
  inputTokens: number;
  outputTokens: number;
  cacheCreationTokens?: number;
  cacheReadTokens?: number;
}