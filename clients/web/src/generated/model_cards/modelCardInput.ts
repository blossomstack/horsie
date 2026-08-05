
/**
 * Create input for a model card.
 */
export interface ModelCardInput {
  modelId: string;
  name: string;
  contextWindow?: number;
  maxTokens?: number;
  /**
   * Canonical thinking-effort values this model supports, ascending.
   */
  thinkingEfforts?: string[];
  /**
   * The provider&#39;s default effort, when documented.
   */
  defaultThinkingEffort?: string;
  /**
   * Wire encoding for this model&#39;s thinking control.
   */
  thinkingDialect?: string;
  /**
   * Where this model is officially served (e.g. &#34;https://api.deepseek.com&#34;).
   */
  baseUrl?: string;
  /**
   * This backend rejects a pinned `tool_choice` while thinking is enabled.
   */
  forcedToolsDisableThinking?: boolean;
}