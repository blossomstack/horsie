
/**
 * An access token and the refresh token that replaces it. The refresh token
 * rotates on every use.
 */
export interface TokenPair {
  accessToken: string;
  refreshToken: string;
  /**
   * Seconds until the access token expires.
   */
  expiresIn: number;
}