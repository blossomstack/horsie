
/**
 * One configured model alias.
 */
export interface ModelView {
  /**
   * The alias sessions select (e.g. &quot;sonnet&quot;).
   */
  alias: string;
  /**
   * Name of the provider this model routes to.
   */
  provider: string;
  /**
   * The provider&#x27;s model identifier (e.g. &quot;claude-sonnet-4-6&quot;).
   */
  modelId: string;
  maxTokens?: number;
  /**
   * The model&#x27;s total context window, in tokens. A built-in default is
   */
  contextWindow?: number;
  /**
   * Canonical thinking-effort values this model offers, in ascending order.
   */
  thinkingEfforts?: string[];
  /**
   * Default applied when a session does not choose one.
   */
  thinkingEffort?: string;
  /**
   * Wire encoding for this model&#x27;s thinking control.
   */
  thinkingDialect?: string;
}