
/**
 * velos config to persist. Only `server_url`, `image`, and `advertise_host`
 */
export interface VelosInput {
  serverUrl: string;
  image: string;
  advertiseHost: string;
  token?: string;
  tokenEnv?: string;
  runtimeBin?: string;
  workspaceRoot?: string;
  listen?: string;
  cpu?: number;
  memoryMib?: number;
  connectTimeoutSecs?: number;
  /**
   * Server HTTP port reachable from the worker network at `advertise_host`
   */
  httpPort?: number;
}