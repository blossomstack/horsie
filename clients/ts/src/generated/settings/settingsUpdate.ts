
import { ModelInput } from './modelInput';
import { ProviderInput } from './providerInput';
import { VendorInput } from './vendorInput';
/**
 * Replace the runtime-editable settings. Each present field fully replaces
 */
export interface SettingsUpdate {
  providers?: ProviderInput[];
  models?: ModelInput[];
  vendors?: VendorInput[];
  defaultVendor?: string;
}