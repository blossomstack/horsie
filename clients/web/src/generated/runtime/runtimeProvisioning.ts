
/**
 * Announces that the runtime is executing provision steps before Ready. The
 * executor extends its handshake deadline and expects Ready or
 * ProvisionFailed next on this connection.
 */
export interface RuntimeProvisioning {
  runtimeId: string;
}