
/**
 * A stored model card.
 */
export interface ModelCard {
  /**
   * Official provider model id — the card&#x27;s identity (e.g. &quot;claude-sonnet-4-6&quot;).
   */
  modelId: string;
  /**
   * Display label (e.g. &quot;Claude Sonnet 4.6&quot;).
   */
  name: string;
  /**
   * Total context window in tokens.
   */
  contextWindow?: number;
  /**
   * Generation cap in tokens.
   */
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
   * This backend rejects a pinned `tool_choice` while thinking is enabled,
   */
  forcedToolsDisableThinking?: boolean;
  createdAt: string;
  updatedAt: string;
}