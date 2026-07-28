
/**
 * Agent settings supplied at session creation.
 */
export interface AgentSettings {
  model: string;
  allowedTools?: string[];
  usePlugins?: boolean;
  maxIterations?: number;
  maxRetries?: number;
  /**
   * Names of enabled MCP servers this session may call, namespaced
   */
  mcpServers?: string[];
  /**
   * Memory spaces this session may read and write; absent → none, and the
   */
  memorySpaces?: string[];
  /**
   * Canonical thinking effort for this session, chosen from the model&#x27;s
   */
  thinkingEffort?: string;
}