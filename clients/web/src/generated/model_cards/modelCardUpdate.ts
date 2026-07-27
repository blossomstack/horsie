
/**
 * Update input — `model_id` is immutable (rename = delete + create).
 */
export interface ModelCardUpdate {
  name: string;
  contextWindow?: number;
  maxTokens?: number;
}