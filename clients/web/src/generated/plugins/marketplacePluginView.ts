
/**
 * One plugin a marketplace offers, as cached from its index.
 */
export interface MarketplacePluginView {
  name: string;
  description?: string;
  version?: string;
  /**
   * True when a bundle installed from this entry is already in the library,
   */
  installed: boolean;
}