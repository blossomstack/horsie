
/**
 * Token usage for a model turn
 */
export interface Usage {
  inputTokens: number;
  outputTokens: number;
  /**
   * Tokens written to a new prompt-cache entry this turn (Anthropic only;
   */
  cacheCreationTokens?: number;
  /**
   * Tokens served from an existing prompt-cache entry this turn
   */
  cacheReadTokens?: number;
}