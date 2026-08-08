
/**
 * Token usage for a model turn
 */
export interface Usage {
  /**
   * The full size of the prompt sent to the model, normalized so this
   * means the same thing on every wire: it INCLUDES any cache-read/
   * cache-creation tokens (Anthropic's own wire reports those separately
   * from its "input" count, so the Anthropic provider adds them back in
   * here — OpenAI's `prompt_tokens` already includes `cached_tokens`, so
   * no adjustment is needed there). The cache fields below are a cost
   * breakdown of this total, not additional tokens on top of it.
   */
  inputTokens: number;
  outputTokens: number;
  /**
   * Tokens written to a new prompt-cache entry this turn (Anthropic only;
   * billed at a premium). Absent when the provider reports no cache data.
   */
  cacheCreationTokens?: number;
  /**
   * Tokens served from an existing prompt-cache entry this turn
   * (Anthropic + OpenAI-compatible `cached_tokens`; billed at a discount).
   * Absent when the provider reports no cache data.
   */
  cacheReadTokens?: number;
}