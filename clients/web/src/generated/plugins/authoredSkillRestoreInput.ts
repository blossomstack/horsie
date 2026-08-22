
/**
 * Roll a skill back to one of its own past revisions. The restore is itself a
 * new revision, so nothing in the history is lost by undoing.
 */
export interface AuthoredSkillRestoreInput {
  plugin: string;
  name: string;
  revision: number;
}