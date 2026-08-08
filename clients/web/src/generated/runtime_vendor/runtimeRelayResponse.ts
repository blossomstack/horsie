
import { RuntimeOutboundMessage } from '../runtime';
/**
 * A runtime's own reply, forwarded verbatim.
 *
 * The union permits `Ready`/`Provisioning`/`ProvisionFailed` because it wraps
 * `RuntimeOutboundMessage` whole, but those never cross this link in practice:
 * they arrive during a runtime's dial-back handshake, before any request has
 * been relayed, and the vendor consumes them itself.
 */
export interface RuntimeRelayResponse {
  runtimeId: string;
  message: RuntimeOutboundMessage;
}