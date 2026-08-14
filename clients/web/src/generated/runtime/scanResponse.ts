
import { PluginAgent } from './pluginAgent';
import { PluginSkill } from './pluginSkill';
import { WorkspaceScan } from './workspaceScan';
export interface ScanResponse {
  callId: string;
  workspaces: WorkspaceScan[];
  sharedSkills: PluginSkill[];
  /**
   * Agent definitions from the same library. Optional so an older runtime
   * binary still deserializes against a newer server.
   */
  sharedAgents?: PluginAgent[];
  /**
   * Absolute root of the calling agent's plugin tree. Optional so an older
   * runtime binary still deserializes against a newer server; a current one
   * always reports it, because the agent was provisioned before it scanned.
   */
  sharedRoot?: string;
}