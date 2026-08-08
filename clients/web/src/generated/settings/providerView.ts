
/**
 * One configured LLM provider, redacted for display.
 */
export interface ProviderView {
  /**
   * Provider name — the key a model's `provider` references.
   */
  name: string;
  /**
   * Provider kind: "anthropic" or "openai".
   */
  kind: string;
  baseUrl?: string;
  /**
   * Whether this provider can authenticate at all: a ChatGPT plan is signed
   * in, any other kind has a stored `api_key`. Which of the two applies
   * follows from `kind`, so one flag serves both — and a provider that
   * cannot authenticate is one no model should be added to.
   */
  hasCredential: boolean;
  /**
   * Retain thinking-block signatures from this provider. Required for
   * genuine Anthropic (it validates them on replay); off for
   * Anthropic-compatible endpoints, which do not.
   */
  keepThinkingSignature: boolean;
}