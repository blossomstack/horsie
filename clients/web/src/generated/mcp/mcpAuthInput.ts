
import { McpBearerInput } from './mcpBearerInput';
import { McpGithubAppAuth } from './mcpGithubAppAuth';
import { McpNoAuth } from './mcpNoAuth';
/**
 * Auth input for an upsert. Add an `OAuth` variant in the OAuth phase.
 */
export type McpAuthInput =
  | { kind: "None"; value: McpNoAuth }
  | { kind: "Bearer"; value: McpBearerInput }
  | { kind: "GithubApp"; value: McpGithubAppAuth };