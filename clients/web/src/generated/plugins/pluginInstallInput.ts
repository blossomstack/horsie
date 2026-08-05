
/**
 * Install a bundle, or register the catalogue a URL turned out to be.
 */
export interface PluginInstallInput {
  sourceUrl?: string;
  sourceRef?: string;
  marketplace?: string;
  pluginName?: string;
}