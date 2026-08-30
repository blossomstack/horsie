
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
   * The provider's default effort, when documented.
   */
  defaultThinkingEffort?: string;
  /**
   * Wire encoding for this model's thinking control.
   */
  thinkingDialect?: string;
  /**
   * Where this model is officially served (e.g. "https://api.deepseek.com").
   */
  baseUrl?: string;
  /**
   * This backend rejects a pinned `tool_choice` while thinking is enabled.
   * Absent means false.
   */
  forcedToolsDisableThinking?: boolean;
  /**
   * This model can be shown images. Absent means false.
   */
  supportsImages?: boolean;
  /**
   * This model can be shown documents (PDFs). Separate from
   * `supports_images` because the wires differ per provider and per model.
   * Absent means false.
   */
  supportsDocuments?: boolean;
}