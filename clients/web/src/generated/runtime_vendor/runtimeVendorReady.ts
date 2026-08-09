
import { RuntimeVendorCapabilities } from './runtimeVendorCapabilities';
/**
 * First message a vendor sends, mirroring `RuntimeReady`. `vendor_name` is
 * the name sessions select by.
 *
 * `instance_id` identifies the *process*, not the vendor: one value generated
 * at startup and repeated on every reconnect. It exists so the server can tell
 * an agent reclaiming its own name after a dropped socket from a second agent
 * trying to take that name over, and it is used for nothing else — a vendor is
 * addressed by name everywhere else in the system.
 */
export interface RuntimeVendorReady {
  vendorName: string;
  instanceId: string;
  capabilities: RuntimeVendorCapabilities;
}