
/**
 * A model alias to persist.
 */
export interface ModelInput {
  alias: string;
  provider: string;
  modelId: string;
  maxTokens?: number;
  contextWindow?: number;
  thinkingEfforts?: string[];
  thinkingEffort?: string;
  thinkingDialect?: string;
  /**
   * This backend rejects a pinned `tool_choice` while thinking is enabled,
   * so thinking is disabled for those requests. Absent means false.
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