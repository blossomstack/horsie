
import { CapabilitySpec } from '../capabilities';
import { AgentSettings } from '../session';
import { RepoConfig } from './repoConfig';
export interface CreateSessionRequest {
  name?: string;
  agent: AgentSettings;
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
  /**
   * Selected plugin-bundle names to provision for this session; absent →
   */
  plugins?: string[];
}