
import { RuntimeVendorEvent } from './runtimeVendorEvent';
/**
 * request_id echoes the request's for responses; a fresh UUID for
 * unsolicited announcements (`Ready`, `RuntimeStateChanged`).
 */
export interface RuntimeVendorOutboundMessage {
  requestId: string;
  event: RuntimeVendorEvent;
}