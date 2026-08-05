
import { MarketplaceView } from './marketplaceView';
import { PluginView } from './pluginView';
/**
 * What a pasted URL turned out to be.
 */
export type InstallOutcome =
  | { outcome: "Installed"; value: PluginView }
  | { outcome: "Marketplace"; value: MarketplaceView };