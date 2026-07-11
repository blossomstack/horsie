
import { Access } from './access';
/**
 * A directory-subtree grant (`nono::allow_path`).
 */
export interface DirGrant {
  path: string;
  access: Access;
}