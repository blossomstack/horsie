
/**
 * A provider to persist. Replaces any provider of the same `name`.
 */
export interface ProviderInput {
  name: string;
  /**
   * Provider kind: &#34;anthropic&#34; or &#34;openai&#34;.
   */
  kind: string;
  baseUrl?: string;
  /**
   * New inline key. Omit to keep the existing stored key; &#34;&#34; to clear.
   */
  apiKey?: string;
  /**
   * Retain thinking-block signatures from this provider. Omit for the
   */
  keepThinkingSignature?: boolean;
}