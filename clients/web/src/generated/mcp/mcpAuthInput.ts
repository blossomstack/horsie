
import { McpBearerInput } from './mcpBearerInput';
import { McpGithubAppAuth } from './mcpGithubAppAuth';
import { McpNoAuth } from './mcpNoAuth';
import { McpOAuthInput } from './mcpOAuthInput';
/**
 * Auth input for an upsert.
 */
export type McpAuthInput =
  | { kind: "None"; value: McpNoAuth }
  | { kind: "Bearer"; value: McpBearerInput }
  | { kind: "OAuth"; value: McpOAuthInput }
  | { kind: "GithubApp"; value: McpGithubAppAuth };