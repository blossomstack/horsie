
/**
 * A provider to persist. Replaces any provider of the same `name`.
 */
export interface ProviderInput {
  name: string;
  /**
   * Provider kind: &quot;anthropic&quot; or &quot;openai&quot;.
   */
  kind: string;
  baseUrl?: string;
  /**
   * New inline key. Omit to keep the existing stored key; &quot;&quot; to clear.
   */
  apiKey?: string;
  /**
   * Retain thinking-block signatures from this provider. Omit for the
   */
  keepThinkingSignature?: boolean;
}