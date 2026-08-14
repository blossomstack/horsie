
import { DirGrant } from './dirGrant';
import { FileGrant } from './fileGrant';
import { TempDirGrant } from './tempDirGrant';
import { WorkingDirGrant } from './workingDirGrant';
/**
 * A single capability grant. The kind is explicit so directory-vs-file intent is
 * unambiguous; `nono`'s `allow_path` is directory-only and `allow_file` single-file.
 */
export type Grant =
  | { type: "Dir"; value: DirGrant }
  | { type: "File"; value: FileGrant }
  | { type: "WorkingDir"; value: WorkingDirGrant }
  | { type: "TempDir"; value: TempDirGrant };