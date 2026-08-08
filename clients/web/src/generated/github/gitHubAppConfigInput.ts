
/**
 * GitHub App config to persist. Write-only secrets: omit to keep the stored
 * value, "" to clear, a value to set.
 */
export interface GitHubAppConfigInput {
  clientId: string;
  clientSecret?: string;
  appId?: number;
  privateKey?: string;
  appSlug?: string;
  callbackBase?: string;
}