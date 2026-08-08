
/**
 * The authorize URL to redirect the browser to, returned by `connect` for an
 * `oauth` server after discovery + (if needed) dynamic client registration.
 */
export interface McpAuthorizeUrl {
  url: string;
}