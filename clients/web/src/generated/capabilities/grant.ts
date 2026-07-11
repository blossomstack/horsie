
import { DirGrant } from './dirGrant';
import { FileGrant } from './fileGrant';
import { WorkingDirGrant } from './workingDirGrant';
/**
 * A single capability grant. The kind is explicit so directory-vs-file intent is
 */
export type Grant =
  | { type: "Dir"; value: DirGrant }
  | { type: "File"; value: FileGrant }
  | { type: "WorkingDir"; value: WorkingDirGrant };