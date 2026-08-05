
import { PluginAgent } from './pluginAgent';
import { PluginCommand } from './pluginCommand';
import { PluginSkill } from './pluginSkill';
import { WorkspaceScan } from './workspaceScan';
export interface ScanResponse {
  callId: string;
  workspaces: WorkspaceScan[];
  sharedSkills: PluginSkill[];
  /**
   * Agent definitions from the same library. Optional so an older runtime
   */
  sharedAgents?: PluginAgent[];
  /**
   * Slash commands from the same library. Optional for the same reason
   */
  sharedCommands?: PluginCommand[];
  /**
   * Absolute path of the shared plugin library root, when one is configured
   */
  sharedRoot?: string;
}