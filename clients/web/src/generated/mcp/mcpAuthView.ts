
import { McpBearerView } from './mcpBearerView';
import { McpGithubAppAuth } from './mcpGithubAppAuth';
import { McpNoAuth } from './mcpNoAuth';
import { McpOAuthView } from './mcpOAuthView';
/**
 * Redacted auth view for a configured server.
 */
export type McpAuthView =
  | { kind: "None"; value: McpNoAuth }
  | { kind: "Bearer"; value: McpBearerView }
  | { kind: "OAuth"; value: McpOAuthView }
  | { kind: "GithubApp"; value: McpGithubAppAuth };