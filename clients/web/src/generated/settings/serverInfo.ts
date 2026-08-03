
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
  /**
   * Where actor journals are stored: &quot;file&quot; (JSONL under data_dir) or
   */
  journalBackend: string;
}