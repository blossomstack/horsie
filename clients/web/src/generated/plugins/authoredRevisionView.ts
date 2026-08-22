
import { AuthoredFileView } from './authoredFileView';
/**
 * One past state of a skill. A deleted skill keeps its history: the tombstone
 * is a revision like any other, so "what did this say before someone removed
 * it" is answerable.
 */
export interface AuthoredRevisionView {
  revision: number;
  description: string;
  body: string;
  files: AuthoredFileView[];
  deleted: boolean;
  createdAt: string;
}