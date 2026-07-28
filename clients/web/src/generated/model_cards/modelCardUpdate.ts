
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
}