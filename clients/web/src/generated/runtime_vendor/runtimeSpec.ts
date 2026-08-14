
import { EnvVar } from '../executor';
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
   * Resolved values only the server can mint: the runtime's dial token, the
   * plugin bundle manifest, and the session's own environment variables.
   *
   * The dial token is the only credential here, and deliberately so. It does
   * not expire, which matters because a vendor whose substrate cannot
   * rewrite a running machine's environment freezes whatever this carried at
   * create time. Anything with a lifetime — a GitHub token, bundle access —
   * is fetched by the runtime against that token instead.
   */
  env: EnvVar[];
}