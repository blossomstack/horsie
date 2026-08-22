
import { CatalogEntryView } from './catalogEntryView';
import { PluginKind } from './pluginKind';
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
   * Resolved version (manifest version, else the cloned commit sha, else an
   * authored plugin's generation).
   */
  version?: string;
  kind: PluginKind;
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
  /**
   * Size of the packed zip. For an authored bundle this is the size of the
   * package as last rendered.
   */
  artifactSize: number;
}