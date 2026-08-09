
import { VendorCapabilities } from './vendorCapabilities';
/**
 * A runtime vendor sessions can target.
 *
 * A live roster rather than a settings record: a vendor process appears once
 * it completes the handshake and disappears when its link drops, and a vendor
 * configured on the server appears as soon as it is saved. What a configured
 * one is made of is not here — see `runtime_vendor.RuntimeVendorConfigView`.
 */
export interface VendorView {
  /**
   * The name sessions select by.
   */
  name: string;
  /**
   * Whether new sessions default to this vendor.
   */
  isDefault: boolean;
  /**
   * What this vendor can do with a session workspace.
   */
  capabilities: VendorCapabilities;
}