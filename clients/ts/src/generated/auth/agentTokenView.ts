
/**
 * A machine token as listed in Settings. The secret itself appears exactly
 * once, in `AgentTokenCreated` — only its hash is stored, so there is nothing
 * to show later even if we wanted to.
 */
export interface AgentTokenView {
  id: string;
  label: string;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  /**
   * Unix epoch seconds, absent until the token is first used.
   */
  lastUsedAt?: string;
}