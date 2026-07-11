
/**
 * One configured LLM provider, redacted for display.
 */
export interface ProviderView {
  /**
   * Provider name — the key a model&#x27;s `provider` references.
   */
  name: string;
  /**
   * Provider kind, e.g. &quot;anthropic&quot; (the only kind today).
   */
  kind: string;
  baseUrl?: string;
  /**
   * Env var the API key is read from, when configured that way.
   */
  apiKeyEnv?: string;
  /**
   * True when an inline `api_key` is stored.
   */
  hasInlineKey: boolean;
}