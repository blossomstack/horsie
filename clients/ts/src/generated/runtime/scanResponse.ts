
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
   * Absolute path of the shared plugin library root, when one is configured
   * and the request asked for it. Optional so an older runtime binary still
   * deserializes against a newer server.
   */
  sharedRoot?: string;
}