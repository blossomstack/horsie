
/**
 * Cumulative token usage across a session's completed turns. `u64` (not the
 * per-turn `Usage`'s `u32`) because it sums re-sent context over many turns.
 * Cache fields are `Option<u64>`, absent only when no completed turn ever
 * reported cache data — never coerced to a fake zero.
 */
export interface UsageView {
  inputTokens: number;
  outputTokens: number;
  cacheCreationTokens?: number;
  cacheReadTokens?: number;
}