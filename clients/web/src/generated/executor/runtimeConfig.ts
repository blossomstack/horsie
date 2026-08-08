
import { EnvVar } from './envVar';
import { ProvisionStep } from './provisionStep';
import { WorkspaceConfig } from './workspaceConfig';
/**
 * Runtime configuration
 */
export interface RuntimeConfig {
  workspaces: WorkspaceConfig[];
  /**
   * Directories prepended to PATH when running plugin hooks (e.g. the node bin
   * dir), resolved by the CLI. Empty inherits the ambient PATH.
   */
  hookPath: string[];
  /**
   * Environment variables explicitly injected into the runtime child by the
   * daemon (job-scoped values like capability tokens or a synthetic `HOME`).
   * Applied after the sandbox env scrub, so injection wins over the ambient
   * allowlist on conflict. Empty injects nothing.
   */
  env: EnvVar[];
  /**
   * Setup steps the runtime executes before the agent loop (vendor-injected
   * into the child via the HORSIE_PROVISION env var). Empty runs nothing.
   */
  provision: ProvisionStep[];
  /**
   * Where the runtime mirrors its per-agent cwd/env map, so a respawn
   * resumes with it intact. Set only by a vendor that can respawn a runtime;
   * absent keeps that state in memory for the life of the process.
   */
  stateFile?: string;
}