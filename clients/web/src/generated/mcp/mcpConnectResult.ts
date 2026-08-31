
/**
 * The result of a connect/smoke test (`initialize` + `tools/list`).
 */
export interface McpConnectResult {
  ok: boolean;
  toolCount?: number;
  /**
   * What the server called itself in the handshake, so a successful test can
   * show what it just learned.
   */
  discoveredTitle?: string;
  error?: string;
}