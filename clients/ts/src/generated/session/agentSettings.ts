
/**
 * Agent settings supplied at session creation.
 */
export interface AgentSettings {
  model: string;
  systemPrompt?: string;
  allowedTools?: string[];
  usePlugins?: boolean;
  maxIterations?: number;
  maxRetries?: number;
  /**
   * Names of enabled MCP servers this session may call, namespaced
   */
  mcpServers?: string[];
}