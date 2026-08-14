
export interface ScanRequest {
  callId: string;
  /**
   * Whose plugin tree to read, and the same id `ToolCallRequest` carries.
   * Required: a scan without one could only read a tree shared by every
   * agent, which is the thing per-agent provisioning exists to end.
   *
   * `include_shared` used to sit here as an opt-in. It is gone: an agent that
   * loads no plugins is provisioned with an empty bundle set, so its tree is
   * empty and there is nothing to gate. One fact, said once.
   */
  agentId: string;
  workspace?: string;
  instructionCandidates: string[];
  skillsGlob: string;
}