
import { CatalogEntryView } from './catalogEntryView';
/**
 * A library entry as shown in the web UI (metadata only — never the bytes).
 */
export interface PluginView {
  /**
   * Canonical bundle name (from plugin.json, else the marketplace entry's
   * name, else the repo basename).
   */
  name: string;
  description?: string;
  /**
   * Resolved version (manifest version, else the cloned commit sha).
   */
  version?: string;
  sourceUrl: string;
  sourceRef?: string;
  /**
   * Everything the bundle offers, sorted by kind then name.
   */
  catalog: CatalogEntryView[];
  /**
   * Whether the bundle ships hooks horsie will run.
   */
  hasHooks: boolean;
  /**
   * Pre-checked in the new-session bundle picker.
   */
  enabledDefault: boolean;
  artifactSize: number;
  /**
   * The marketplace this bundle came from, when it came from one. A bundle
   * installed from a plain git URL has none.
   */
  marketplace?: string;
}