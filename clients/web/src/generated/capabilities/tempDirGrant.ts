
import { Access } from './access';
/**
 * The process temp directory, resolved at apply time rather than written as a
 * path: on macOS `TMPDIR` is a per-user `/var/folders/…/T` that no static spec
 * can name, and the runtime inherits it through the env allowlist.
 */
export interface TempDirGrant {
  access: Access;
}