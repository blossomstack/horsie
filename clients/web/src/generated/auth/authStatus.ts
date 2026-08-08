
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
}