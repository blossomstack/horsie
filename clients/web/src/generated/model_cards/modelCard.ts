
/**
 * A stored model card.
 */
export interface ModelCard {
  /**
   * Official provider model id — the card's identity (e.g. "claude-sonnet-4-6").
   */
  modelId: string;
  /**
   * Display label (e.g. "Claude Sonnet 4.6").
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
   * The provider's default effort, when documented.
   */
  defaultThinkingEffort?: string;
  /**
   * Wire encoding for this model's thinking control.
   */
  thinkingDialect?: string;
  /**
   * Where this model is officially served (e.g. "https://api.deepseek.com").
   * Reference data only — nothing reads it, and an operator's configured
   * provider base URL always wins.
   */
  baseUrl?: string;
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
  createdAt: string;
  updatedAt: string;
}