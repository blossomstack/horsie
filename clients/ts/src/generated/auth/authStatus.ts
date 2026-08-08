
/**
 * What the UI needs to decide between rendering the app and rendering a login
 * page. `must_change_password` is only ever true for an authenticated caller
 * — telling an anonymous one that a deployment still has its first-boot
 * password just tells an attacker where to aim.
 */
export interface AuthStatus {
  /**
   * False when the deployment runs with authentication turned off, in which
   * case the UI shows no login surface at all.
   */
  enabled: boolean;
  authenticated: boolean;
  mustChangePassword: boolean;
  /**
   * True when a layer in front of the server owns identity. horsie itself
   * never sets this — it serves no credential routes in that mode, so this
   * endpoint is answered by that layer. The field exists because the UI is
   * horsie's and has to render for such a deployment: it hides every
   * credential control it cannot honour.
   */
  external: boolean;
  /**
   * Where to send someone who is not signed in. Only meaningful when
   * `external`; the UI navigates here instead of rendering a login form.
   */
  loginUrl?: string;
  /**
   * Where to finish signing out. Only meaningful when `external`. Clearing
   * the local session is not enough on its own — the provider holds one
   * too, and leaving it alive signs nobody out of a shared machine.
   */
  logoutUrl?: string;
}