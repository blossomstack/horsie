
/**
 * An authored skill without its body: what a list needs, and what the agent's
 * index shows.
 */
export interface AuthoredSkillSummary {
  plugin: string;
  name: string;
  description: string;
  /**
   * Head revision. Every save appends one.
   */
  revision: number;
  updatedAt: string;
}