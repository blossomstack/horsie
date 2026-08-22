
/**
 * Agent settings supplied at session creation.
 */
export interface AgentSettings {
  model: string;
  /**
   * The built-in tools this session's agents may call, by name. Absent → the
   * default set: every built-in group except the control plane.
   *
   * An allowlist over built-ins only. Skills, MCP servers and memory spaces
   * are selected by their own fields and are never removed by narrowing this
   * one — their names are not fixed at build time, so a selection could not
   * honestly speak for them.
   *
   * Naming a `horsie_*` tool is the whole authorisation for the control
   * plane: authority over this server is asked for, never inherited from a
   * field left unset. Only the main agent gets those tools — subagents,
   * sub sessions and workflow steps inherit the selection but not the
   * authority, the same rule that keeps session-metadata tools off them.
   */
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
}