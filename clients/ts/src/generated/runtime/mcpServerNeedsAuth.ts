
/**
 * It answered `401`. `resource_metadata` is the RFC 9728 URL its
 */
export interface McpServerNeedsAuth {
  server: string;
  resourceMetadata?: string;
}