
import { RepoConfig } from '../session_api';
/**
 * Create or fully replace an agent preset. Omitted list fields default to
 */
export interface AgentPresetInput {
  name: string;
  description?: string;
  vendor?: string;
  model: string;
  repos?: RepoConfig[];
  plugins?: string[];
  mcpServers?: string[];
  memorySpaces?: string[];
  thinkingEffort?: string;
}