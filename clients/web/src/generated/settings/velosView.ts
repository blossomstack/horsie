
/**
 * The velos remote-runtime config, redacted (no token).
 */
export interface VelosView {
  serverUrl: string;
  image: string;
  advertiseHost: string;
  tokenEnv?: string;
  hasInlineToken: boolean;
  runtimeBin: string;
  workspaceRoot: string;
  listen: string;
  cpu: number;
  memoryMib: number;
  connectTimeoutSecs: number;
}