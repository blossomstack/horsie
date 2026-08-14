
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
  instructions?: string;
  model: string;
  plugins?: string[];
  mcpServers?: string[];
  memorySpaces?: string[];
  thinkingEffort?: string;
  /**
   * Seeds `AgentSettings.auto_compact` for sessions created from this
   * preset; absent → yes.
   */
  autoCompact?: boolean;
  /**
   * Seeds `AgentSettings.control_plane`; absent → no. Enabling this is the
   * whole authorisation — a session from this preset can then change or
   * delete anything this account owns, without confirming first.
   */
  controlPlane?: boolean;
}