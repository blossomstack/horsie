
import { AuthoredFileView } from './authoredFileView';
/**
 * Create or replace one skill. Supplied fields are replaced wholesale; omitted
 * ones are left as they were, so a caller fixing a typo in the description
 * does not have to resend the body.
 *
 * Creating a skill requires `description` and `body`: a skill a picker cannot
 * label and a model cannot choose between is not one, and the reader refuses
 * it at both ends anyway.
 */
export interface AuthoredSkillWriteInput {
  plugin: string;
  name: string;
  description?: string;
  body?: string;
  files?: AuthoredFileView[];
}