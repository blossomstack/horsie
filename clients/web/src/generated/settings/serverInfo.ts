
/**
 * Read-only deployment paths and version, for transparency. `config_path` is
 */
export interface ServerInfo {
  configPath: string;
  database: string;
  stateDir: string;
  dataDir: string;
  pluginsDir: string;
  version: string;
}