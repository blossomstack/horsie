
import { VendorConfigView } from './vendorConfigView';
/**
 * A runtime vendor sessions can target. `local` is built-in and carries no
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
   * Kind-specific config, redacted. Absent for the built-in `local` vendor.
   */
  config?: VendorConfigView;
  /**
   * The last build/reconfigure failure for this vendor, if any. `None`
   */
  error?: string;
}