
/**
 * The velos remote-runtime config, redacted (no token).
 */
export interface VelosView {
  serverUrl: string;
  image: string;
  /**
   * `host:port` the runtime dials back on — the server&#x27;s externally reachable
   */
  advertiseAddress: string;
  hasInlineToken: boolean;
  runtimeBin: string;
  workspaceRoot: string;
  cpu: number;
  memoryMib: number;
  connectTimeoutSecs: number;
}