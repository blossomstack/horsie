
/**
 * Read-only deployment paths and version, for transparency. `config_path` is
 * the deployment config file (config.json); `database` is where the editable
 * settings above are stored — the two never overlap.
 */
export interface ServerInfo {
  configPath: string;
  database: string;
  stateDir: string;
  dataDir: string;
  pluginsDir: string;
  version: string;
}