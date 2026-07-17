
/**
 * The velos remote-runtime config, redacted (no token).
 */
export interface VelosView {
  serverUrl: string;
  image: string;
  advertiseHost: string;
  hasInlineToken: boolean;
  runtimeBin: string;
  workspaceRoot: string;
  listen: string;
  cpu: number;
  memoryMib: number;
  connectTimeoutSecs: number;
  /**
   * Server HTTP port reachable from the worker network at `advertise_host`;
   */
  httpPort?: number;
}