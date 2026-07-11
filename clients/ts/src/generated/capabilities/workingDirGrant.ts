
import { Access } from './access';
/**
 * The runtime working directory, resolved to `--working-dir` at apply time.
 */
export interface WorkingDirGrant {
  access: Access;
}