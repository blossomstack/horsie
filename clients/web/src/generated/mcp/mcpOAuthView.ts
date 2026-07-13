
/**
 * Redacted OAuth 2.1 auth view. `connected` = a stored access token exists;
 */
export interface McpOAuthView {
  connected: boolean;
  clientId?: string;
  hasClientSecret: boolean;
}