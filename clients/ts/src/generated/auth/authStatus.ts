
/**
 * What the UI needs to decide between rendering the app and rendering a login
 */
export interface AuthStatus {
  /**
   * False when the deployment runs with authentication turned off, in which
   */
  enabled: boolean;
  authenticated: boolean;
  mustChangePassword: boolean;
}