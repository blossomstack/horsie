
import { RuntimeOutboundMessage } from '../runtime';
/**
 * A runtime's own reply, forwarded verbatim.
 *
 * The union permits `Ready` because it wraps `RuntimeOutboundMessage` whole,
 * but it never crosses this link in practice: it arrives during a runtime's
 * dial-back handshake, before any request has been relayed, and the vendor
 * consumes it itself.
 */
export interface RuntimeRelayResponse {
  runtimeId: string;
  message: RuntimeOutboundMessage;
}