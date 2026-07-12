
/**
 * The result of a connect/smoke test (`initialize` + `tools/list`).
 */
export interface McpConnectResult {
  ok: boolean;
  toolCount?: number;
  error?: string;
}