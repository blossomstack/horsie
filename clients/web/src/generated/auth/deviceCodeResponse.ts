
/**
 * What the CLI gets when it starts a device authorization. The device code is
 * the secret it polls with; the user code is what a human reads out and types.
 */
export interface DeviceCodeResponse {
  deviceCode: string;
  userCode: string;
  verificationUri: string;
  /**
   * The same page with the code pre-filled, for when the CLI can print a
   * link the user can click.
   */
  verificationUriComplete: string;
  /**
   * Seconds until the code expires.
   */
  expiresIn: number;
  /**
   * Seconds the CLI must wait between polls while the code is still
   * pending. Polling faster is answered with `slow_down`.
   */
  interval: number;
}