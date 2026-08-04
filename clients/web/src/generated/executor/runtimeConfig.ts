
import { EnvVar } from './envVar';
import { ProvisionStep } from './provisionStep';
import { WorkspaceConfig } from './workspaceConfig';
/**
 * Runtime configuration
 */
export interface RuntimeConfig {
  workspaces: WorkspaceConfig[];
  /**
   * Shared plugin library root, exposed to agents as the `horsie_shared`
   */
  pluginsDir?: string;
  /**
   * Directories prepended to PATH when running plugin hooks (e.g. the node bin
   */
  hookPath: string[];
  /**
   * Environment variables explicitly injected into the runtime child by the
   */
  env: EnvVar[];
  /**
   * Setup steps the runtime executes before the agent loop (vendor-injected
   */
  provision: ProvisionStep[];
  /**
   * Where the runtime mirrors its per-agent cwd/env map, so a respawn
   */
  stateFile?: string;
}