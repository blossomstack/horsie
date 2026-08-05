
/**
 * A stored model card.
 */
export interface ModelCard {
  /**
   * Official provider model id — the card&#39;s identity (e.g. &#34;claude-sonnet-4-6&#34;).
   */
  modelId: string;
  /**
   * Display label (e.g. &#34;Claude Sonnet 4.6&#34;).
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
   * This backend rejects a pinned `tool_choice` while thinking is enabled,
   */
  forcedToolsDisableThinking?: boolean;
  createdAt: string;
  updatedAt: string;
}