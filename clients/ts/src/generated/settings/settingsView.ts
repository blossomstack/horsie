
import { ModelView } from './modelView';
import { ProviderView } from './providerView';
import { ServerInfo } from './serverInfo';
import { VendorView } from './vendorView';
/**
 * Full settings snapshot — returned by `GET /api/config` and after an update.
 */
export interface SettingsView {
  providers: ProviderView[];
  models: ModelView[];
  vendors: VendorView[];
  defaultRuntimeVendor: string;
  info: ServerInfo;
  /**
   * Always false: every provider/model edit applies live. Retained for wire
   * compatibility.
   */
  restartRequired: boolean;
}