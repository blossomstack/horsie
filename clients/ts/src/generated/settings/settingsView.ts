
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
   * True only when an already-active vendor&#x27;s listener-affecting fields
   */
  restartRequired: boolean;
}