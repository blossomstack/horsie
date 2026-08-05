
/**
 * One configured LLM provider, redacted for display.
 */
export interface ProviderView {
  /**
   * Provider name — the key a model&#39;s `provider` references.
   */
  name: string;
  /**
   * Provider kind: &#34;anthropic&#34; or &#34;openai&#34;.
   */
  kind: string;
  baseUrl?: string;
  /**
   * True when an inline `api_key` is stored.
   */
  hasInlineKey: boolean;
  /**
   * Retain thinking-block signatures from this provider. Required for
   */
  keepThinkingSignature: boolean;
}