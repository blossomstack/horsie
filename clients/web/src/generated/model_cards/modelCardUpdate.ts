
/**
 * Update input — `model_id` is immutable (rename = delete + create).
 */
export interface ModelCardUpdate {
  name: string;
  contextWindow?: number;
  maxTokens?: number;
  /**
   * Canonical thinking-effort values this model supports, ascending.
   */
  thinkingEfforts?: string[];
  /**
   * The provider&#x27;s default effort, when documented.
   */
  defaultThinkingEffort?: string;
  /**
   * Wire encoding for this model&#x27;s thinking control.
   */
  thinkingDialect?: string;
  /**
   * Where this model is officially served (e.g. &quot;https://api.deepseek.com&quot;).
   */
  baseUrl?: string;
  /**
   * This backend rejects a pinned `tool_choice` while thinking is enabled.
   */
  forcedToolsDisableThinking?: boolean;
}