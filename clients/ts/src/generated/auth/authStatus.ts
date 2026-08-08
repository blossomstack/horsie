
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
  /**
   * True when a layer in front of the server owns identity. horsie itself
   */
  external: boolean;
  /**
   * Where to send someone who is not signed in. Only meaningful when
   */
  loginUrl?: string;
  /**
   * Where to finish signing out. Only meaningful when `external`. Clearing
   */
  logoutUrl?: string;
}