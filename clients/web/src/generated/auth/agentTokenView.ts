
/**
 * A machine token as listed in Settings. The secret itself appears exactly
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