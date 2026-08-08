
/**
 * Redacted OAuth 2.1 auth view. `connected` = a stored access token exists;
 * secrets are redacted to flags. `client_id` is safe to surface (public).
 */
export interface McpOAuthView {
  connected: boolean;
  clientId?: string;
  hasClientSecret: boolean;
}