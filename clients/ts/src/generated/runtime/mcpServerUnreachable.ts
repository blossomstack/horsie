
/**
 * It could not be reached at all: no such binary, a refused connection, a
 * declaration that would not parse.
 */
export interface McpServerUnreachable {
  server: string;
  reason: string;
}