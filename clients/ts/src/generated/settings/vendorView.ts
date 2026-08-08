
import { VendorCapabilities } from './vendorCapabilities';
/**
 * A runtime vendor sessions can target.
 *
 * Every vendor is a connected agent now: the server holds no vendor
 * configuration and builds nothing, so this is a live roster rather than a
 * settings record. A vendor appears once its agent completes the handshake
 * and disappears when the link drops — there is no configured-but-inactive
 * state, so no config block and no build error to report.
 */
export interface VendorView {
  /**
   * The name the agent announced, which sessions select by.
   */
  name: string;
  /**
   * Whether new sessions default to this vendor.
   */
  isDefault: boolean;
  /**
   * What the agent announced it can do with a session workspace.
   */
  capabilities: VendorCapabilities;
}