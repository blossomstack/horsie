
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
  /**
   * Standing instructions this session's agent runs under, added to the
   * system prompt as its own section. Set from an agent preset, or directly
   * here; absent → none.
   */
  instructions?: string;
  /**
   * Whether this session summarises older history into a compaction
   * boundary once its context fills; absent → yes.
   *
   * A flag rather than a threshold: the share of the window at which
   * compacting is worthwhile is a property of the model, not of the
   * session, so it stays a server constant that can be retuned centrally
   * instead of a number frozen into everyone's saved settings. Has no
   * effect when the model's card declares no context window — there is
   * then nothing to be a share of.
   */
  autoCompact?: boolean;
  /**
   * Whether this session's main agent may manage the horsie server itself.
   *
   * Absent is off, unlike `auto_compact`: authority over the server is
   * granted explicitly or not at all. Only the main agent gets it —
   * subagents, forks and workflow steps inherit the setting but not the
   * tools, the same rule that keeps session-metadata tools off them.
   */
  controlPlane?: boolean;
}