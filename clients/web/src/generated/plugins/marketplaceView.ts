
import { MarketplacePluginView } from './marketplacePluginView';
/**
 * A registered marketplace and the catalogue it last offered.
 */
export interface MarketplaceView {
  /**
   * The index&#39;s declared name, else the repo basename. Primary key.
   */
  name: string;
  sourceUrl: string;
  sourceRef?: string;
  pluginCount: number;
  /**
   * When the index was last read, epoch millis as a string.
   */
  updatedAt: string;
  plugins: MarketplacePluginView[];
  /**
   * Entries the index declared that could not be parsed. Shown rather than
   */
  skipped: string[];
}