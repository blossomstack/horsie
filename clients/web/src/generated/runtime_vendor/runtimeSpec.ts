
import { EnvVar } from '../executor';
import { ProvisionStep } from '../executor';
/**
 * Everything the server can supply about a runtime. Deliberately minimal:
 * anything the vendor knows better (workspace paths, plugin unpack dirs,
 * artifact base URLs) is resolved vendor-side and never crosses the wire.
 */
export interface RuntimeSpec {
  /**
   * Workspace *names*. The vendor resolves each to a path it owns, and
   * fails the request if it cannot honor one.
   */
  workspaces: string[];
  /**
   * Resolved secrets and handles only the server can mint: the scoped
   * GitHub token, the plugin bundle manifest, the plugins token.
   */
  env: EnvVar[];
  /**
   * Setup steps the runtime executes before its message loop.
   */
  provision: ProvisionStep[];
}