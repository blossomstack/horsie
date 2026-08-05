
/**
 * One configured model alias.
 */
export interface ModelView {
  /**
   * The alias sessions select (e.g. &#34;sonnet&#34;).
   */
  alias: string;
  /**
   * Name of the provider this model routes to.
   */
  provider: string;
  /**
   * The provider&#39;s model identifier (e.g. &#34;claude-sonnet-4-6&#34;).
   */
  modelId: string;
  maxTokens?: number;
  /**
   * The model&#39;s total context window, in tokens. A built-in default is
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
   * This backend rejects a pinned `tool_choice` while thinking is enabled,
   */
  forcedToolsDisableThinking?: boolean;
  /**
   * Wire encoding for this model&#39;s thinking control.
   */
  thinkingDialect?: string;
}