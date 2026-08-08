
import { RuntimeVendorSettings } from './runtimeVendorSettings';
/**
 * Create or fully replace a configured runtime vendor.
 */
export interface RuntimeVendorConfigInput {
  name: string;
  settings: RuntimeVendorSettings;
  /**
   * Omit to keep the stored token. Required when creating.
   */
  credential?: string;
}