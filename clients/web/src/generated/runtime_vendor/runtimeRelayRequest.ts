
import { RuntimeInboundMessage } from '../runtime';
/**
 * The runtime protocol, addressed to one of this vendor's runtimes and
 * forwarded verbatim. Tool calls, cancels, workspace scans, and session-start
 * hooks all travel this way, so adding a runtime capability needs no change
 * here at all.
 *
 * A relayed `CancelCall` draws no response, exactly as in `runtime.fl`.
 */
export interface RuntimeRelayRequest {
  runtimeId: string;
  message: RuntimeInboundMessage;
}