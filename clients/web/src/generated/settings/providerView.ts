
/**
 * One configured LLM provider, redacted for display.
 */
export interface ProviderView {
  /**
   * Provider name — the key a model&#x27;s `provider` references.
   */
  name: string;
  /**
   * Provider kind: &quot;anthropic&quot; or &quot;openai&quot;.
   */
  kind: string;
  baseUrl?: string;
  /**
   * True when an inline `api_key` is stored.
   */
  hasInlineKey: boolean;
}