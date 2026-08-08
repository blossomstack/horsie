
/**
 * It answered `401`. `resource_metadata` is the RFC 9728 URL its
 * `WWW-Authenticate` named, when it named one.
 */
export interface McpServerNeedsAuth {
  server: string;
  resourceMetadata?: string;
}