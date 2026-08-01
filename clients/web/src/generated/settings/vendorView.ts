
import { VendorCapabilities } from './vendorCapabilities';
/**
 * A runtime vendor sessions can target.
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