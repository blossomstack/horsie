
/**
 * Token usage for a model turn
 */
export interface Usage {
  /**
   * The full size of the prompt sent to the model, normalized so this
   */
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