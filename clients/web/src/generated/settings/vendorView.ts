
import { VendorCapabilities } from './vendorCapabilities';
import { VendorConfigView } from './vendorConfigView';
/**
 * A runtime vendor sessions can target: DB-configured, or daemon-registered.
 */
export interface VendorView {
  name: string;
  /**
   * Loaded and usable in the running server right now.
   */
  active: boolean;
  /**
   * Whether new sessions default to this vendor.
   */
  isDefault: boolean;
  /**
   * Kind-specific config, redacted. Absent for a daemon-registered vendor.
   */
  config?: VendorConfigView;
  /**
   * The last build/reconfigure failure for this vendor, if any. `None`
   */
  error?: string;
  /**
   * Announced capabilities of the live vendor instance. `None` when the
   */
  capabilities?: VendorCapabilities;
}