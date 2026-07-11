
/**
 * A model alias to persist.
 */
export interface ModelInput {
  alias: string;
  provider: string;
  modelId: string;
  maxTokens?: number;
}