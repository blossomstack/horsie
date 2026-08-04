
import { PluginSkill } from './pluginSkill';
import { WorkspaceScan } from './workspaceScan';
export interface ScanResponse {
  callId: string;
  workspaces: WorkspaceScan[];
  sharedSkills: PluginSkill[];
  /**
   * Absolute path of the shared plugin library root, when one is configured
   */
  sharedRoot?: string;
}