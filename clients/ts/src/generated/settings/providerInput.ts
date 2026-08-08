
/**
 * A provider to persist. Replaces any provider of the same `name`.
 */
export interface ProviderInput {
  name: string;
  /**
   * Provider kind: "anthropic" or "openai".
   */
  kind: string;
  baseUrl?: string;
  /**
   * New inline key. Omit to keep the existing stored key; "" to clear.
   */
  apiKey?: string;
  /**
   * Retain thinking-block signatures from this provider. Omit for the
   * default (off) — required for genuine Anthropic, which validates them.
   */
  keepThinkingSignature?: boolean;
}