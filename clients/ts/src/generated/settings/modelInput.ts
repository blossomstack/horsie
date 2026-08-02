
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
   */
  forcedToolsDisableThinking?: boolean;
}