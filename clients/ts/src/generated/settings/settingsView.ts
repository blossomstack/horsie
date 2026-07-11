
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
  defaultVendor: string;
  info: ServerInfo;
  /**
   * True when a persisted change (a vendor edit) needs a restart to take
   */
  restartRequired: boolean;
}