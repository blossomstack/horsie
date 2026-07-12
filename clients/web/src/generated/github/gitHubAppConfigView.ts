
/**
 * The GitHub App config, redacted for display. Secrets are reported only as
 */
export interface GitHubAppConfigView {
  clientId: string;
  appId?: number;
  appSlug?: string;
  /**
   * True when a client secret is stored.
   */
  hasClientSecret: boolean;
  /**
   * True when a private key (PEM) is stored.
   */
  hasPrivateKey: boolean;
  /**
   * Public base URL for OAuth callbacks; NULL derives from the request host.
   */
  callbackBase?: string;
}