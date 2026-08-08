
/**
 * OAuth 2.1 auth input. All fields optional: leave everything empty to let
 * horsie discover the authorization server and dynamically register a client
 * (RFC 9728 / 8414 / 7591). Set `client_id`/`client_secret` to reuse a
 * pre-registered client; set the `*_endpoint` fields to override discovery when
 * the server exposes no metadata. Secrets: omit=keep, ""=clear, value=set.
 */
export interface McpOAuthInput {
  clientId?: string;
  clientSecret?: string;
  authorizationEndpoint?: string;
  tokenEndpoint?: string;
  registrationEndpoint?: string;
}