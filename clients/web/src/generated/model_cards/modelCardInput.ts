
/**
 * Create input for a model card.
 */
export interface ModelCardInput {
  modelId: string;
  name: string;
  contextWindow?: number;
  maxTokens?: number;
}