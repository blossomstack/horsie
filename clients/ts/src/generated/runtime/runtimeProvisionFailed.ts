
/**
 * Provision steps failed; the runtime exits after sending this. `message` is
 * surfaced as the create/attach failure reason.
 */
export interface RuntimeProvisionFailed {
  runtimeId: string;
  message: string;
}