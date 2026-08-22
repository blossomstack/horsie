
import { AuthoredFileView } from './authoredFileView';
/**
 * One authored skill in full.
 */
export interface AuthoredSkillView {
  plugin: string;
  name: string;
  description: string;
  body: string;
  files: AuthoredFileView[];
  revision: number;
  updatedAt: string;
}