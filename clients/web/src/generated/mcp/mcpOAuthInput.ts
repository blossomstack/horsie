
/**
 * OAuth 2.1 auth input. All fields optional: leave everything empty to let
 */
export interface McpOAuthInput {
  clientId?: string;
  clientSecret?: string;
  authorizationEndpoint?: string;
  tokenEndpoint?: string;
  registrationEndpoint?: string;
}