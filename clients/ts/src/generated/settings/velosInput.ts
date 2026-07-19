
/**
 * velos config to persist. Only `server_url`, `image`, and `advertise_address`
 */
export interface VelosInput {
  serverUrl: string;
  image: string;
  /**
   * `host:port` the runtime dials back on (the server&#x27;s externally reachable
   */
  advertiseAddress: string;
  token?: string;
  runtimeBin?: string;
  workspaceRoot?: string;
  cpu?: number;
  memoryMib?: number;
  connectTimeoutSecs?: number;
}