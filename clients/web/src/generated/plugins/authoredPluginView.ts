
import { AuthoredSkillSummary } from './authoredSkillSummary';
/**
 * An authored plugin and the skills it currently holds.
 */
export interface AuthoredPluginView {
  name: string;
  description?: string;
  generation: number;
  skills: AuthoredSkillSummary[];
}