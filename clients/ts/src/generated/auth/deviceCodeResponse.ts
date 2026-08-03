
/**
 * What the CLI gets when it starts a device authorization. The device code is
 */
export interface DeviceCodeResponse {
  deviceCode: string;
  userCode: string;
  verificationUri: string;
  /**
   * The same page with the code pre-filled, for when the CLI can print a
   */
  verificationUriComplete: string;
  /**
   * Seconds until the code expires.
   */
  expiresIn: number;
  /**
   * Seconds the CLI must wait between polls while the code is still
   */
  interval: number;
}