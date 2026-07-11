
import { CapabilitySpec } from '../capabilities';
import { AgentSettings } from '../session';
export interface CreateSessionRequest {
  name?: string;
  agent: AgentSettings;
  /**
   * Workspace roots (&gt;=1), like `horsie job run --workdir`.
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
}