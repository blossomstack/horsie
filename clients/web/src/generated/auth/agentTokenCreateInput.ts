
export interface AgentTokenCreateInput {
  /**
   * Which machine this token is for. Required: a wall of unlabelled secrets
   * is unrevokable in practice.
   */
  label: string;
}