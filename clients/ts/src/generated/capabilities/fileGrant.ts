
import { Access } from './access';
/**
 * A single-file grant, e.g. a device node (`nono::allow_file`).
 */
export interface FileGrant {
  path: string;
  access: Access;
}