
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
  createdAt: string;
  updatedAt: string;
}