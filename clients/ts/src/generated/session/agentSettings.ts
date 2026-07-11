
/**
 * Agent settings supplied at session creation.
 */
export interface AgentSettings {
  model: string;
  systemPrompt?: string;
  allowedTools?: string[];
  allowAskUser?: boolean;
  usePlugins?: boolean;
  maxIterations?: number;
  maxRetries?: number;
}