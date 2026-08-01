
import { ModelInput } from './modelInput';
import { ProviderInput } from './providerInput';
/**
 * Replace the runtime-editable settings. Each present field fully replaces
 */
export interface SettingsUpdate {
  providers?: ProviderInput[];
  models?: ModelInput[];
  defaultVendor?: string;
}