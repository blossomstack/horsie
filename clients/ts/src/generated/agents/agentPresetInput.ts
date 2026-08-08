
/**
 * Create or fully replace an agent preset. Omitted list fields default to
 * empty; `description` defaults to "".
 *
 * Named `AgentPresetInput`, not `AgentInput`: fluorite resolves imported types
 * by bare name across packages, so a second `AgentInput` would hijack
 * `events`' reference to the agent-loop `agent.AgentInput`.
 */
export interface AgentPresetInput {
  name: string;
  description?: string;
  model: string;
  plugins?: string[];
  mcpServers?: string[];
  memorySpaces?: string[];
  thinkingEffort?: string;
}