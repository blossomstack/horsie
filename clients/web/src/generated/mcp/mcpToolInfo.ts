
/**
 * One tool a server advertised at its last successful connect. The name is the
 * server's own spelling — the `mcp__<server>__` prefix is added where the tool
 * is offered to a model, not here.
 */
export interface McpToolInfo {
  name: string;
  /**
   * The tool's own description, verbatim. Empty when it published none.
   */
  description: string;
}