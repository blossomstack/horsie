
/**
 * An agent preset as shown to clients.
 */
export interface AgentView {
  /**
   * Slug; the id of record, used in API paths and CLI invocations.
   */
  name: string;
  /**
   * What this preset is for, as shown in the roster. Never sent to the
   * model — `instructions` is what the model reads.
   */
  description: string;
  /**
   * Standing instructions this preset's agent runs under, added to the
   * system prompt as its own section. Absent → the agent behaves exactly
   * like an unpresetted one.
   */
  instructions?: string;
  /**
   * Configured model alias.
   */
  model: string;
  /**
   * Selected plugin-bundle (skill) names.
   */
  plugins: string[];
  /**
   * Enabled MCP server names.
   */
  mcpServers: string[];
  /**
   * Memory spaces the session may read and write.
   */
  memorySpaces: string[];
  /**
   * Canonical thinking effort; absent → the model's configured default.
   */
  thinkingEffort?: string;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}