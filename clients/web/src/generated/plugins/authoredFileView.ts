
/**
 * One file that sits beside a skill's `SKILL.md` — `scripts/run.sh`,
 * `references/api.md`. The path is relative to the skill's own directory.
 */
export interface AuthoredFileView {
  path: string;
  content: string;
}