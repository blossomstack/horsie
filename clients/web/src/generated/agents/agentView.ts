
import { RepoConfig } from '../session_api';
/**
 * An agent preset as shown to clients.
 */
export interface AgentView {
  /**
   * Slug; the id of record, used in API paths and CLI invocations.
   */
  name: string;
  description: string;
  /**
   * Configured model alias.
   */
  model: string;
  /**
   * Repositories cloned into the session workspace at provision time.
   */
  repos: RepoConfig[];
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