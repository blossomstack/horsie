
import { CapabilitySpec } from '../capabilities';
import { AgentSettings } from '../session';
import { RepoConfig } from './repoConfig';
export interface CreateSessionRequest {
  name?: string;
  agent: AgentSettings;
  /**
   * Bring-your-own workspace roots (local vendor only). Mutually exclusive
   */
  workdirs: string[];
  /**
   * Runtime vendor name; defaults to &quot;local&quot;.
   */
  vendor?: string;
  /**
   * Capability spec overriding the server default.
   */
  capabilities?: CapabilitySpec;
  /**
   * Repositories cloned into a vendor-managed workspace at provision time.
   */
  repos?: RepoConfig[];
}