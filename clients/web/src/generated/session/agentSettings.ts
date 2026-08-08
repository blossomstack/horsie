
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
   * `mcp__<name>__<tool>`; absent → none.
   */
  mcpServers?: string[];
  /**
   * Memory spaces this session may read and write; absent → none, and the
   * memory_* tools are not offered.
   */
  memorySpaces?: string[];
  /**
   * Canonical thinking effort for this session, chosen from the model's
   * offered list. Absent → the model's configured default.
   */
  thinkingEffort?: string;
  /**
   * Cap on concurrently-active subagents in this session; absent → the
   * server's built-in default (8).
   */
  maxConcurrentSubagents?: number;
}