
import { RuntimeVendorSettings } from './runtimeVendorSettings';
/**
 * A configured runtime vendor as shown to clients.
 */
export interface RuntimeVendorConfigView {
  /**
   * The name sessions select by. Exclusive with the names dialled-in vendor
   * agents announce.
   */
  name: string;
  settings: RuntimeVendorSettings;
  /**
   * Whether a credential is stored. The token itself never leaves the
   * server — a settings page that could read one back turns every session
   * cookie into a credential exfiltration path.
   */
  hasCredential: boolean;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}