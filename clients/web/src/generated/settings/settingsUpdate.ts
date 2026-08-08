
import { ModelInput } from './modelInput';
import { ProviderInput } from './providerInput';
/**
 * Replace the runtime-editable settings. Each present field fully replaces
 * that section; an omitted (null) field leaves it unchanged. Vendors are
 * absent by design: they are connected agents, configured in their own
 * processes, so there is nothing here to persist.
 */
export interface SettingsUpdate {
  providers?: ProviderInput[];
  models?: ModelInput[];
  defaultVendor?: string;
}