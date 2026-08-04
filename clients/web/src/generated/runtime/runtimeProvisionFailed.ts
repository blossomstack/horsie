
/**
 * Provision steps failed; the runtime exits after sending this. `message` is
 */
export interface RuntimeProvisionFailed {
  runtimeId: string;
  message: string;
}