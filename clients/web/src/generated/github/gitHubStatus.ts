
/**
 * Current GitHub connection state, for the Settings UI.
 */
export interface GitHubStatus {
  /**
   * True once an account is connected (OAuth credentials stored).
   */
  connected: boolean;
  /**
   * Login of the connected account, when connected.
   */
  login?: string;
  /**
   * True once the GitHub App is configured (client id present).
   */
  appConfigured: boolean;
  /**
   * Repos visible to the installation; 0 until the first repo listing.
   */
  repoCount: number;
}