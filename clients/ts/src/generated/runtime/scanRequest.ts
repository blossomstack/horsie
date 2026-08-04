
export interface ScanRequest {
  callId: string;
  workspace?: string;
  instructionCandidates: string[];
  skillsGlob: string;
  /**
   * When true, also enumerate the shared plugin library and return its skills in
   */
  includeShared: boolean;
}