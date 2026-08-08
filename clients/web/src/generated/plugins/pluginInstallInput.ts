
/**
 * Install a bundle, or register the catalogue a URL turned out to be.
 *
 * Exactly one of `source_url` and the `(marketplace, plugin_name)` pair must be
 * given; the other two fields are then absent. Four optional fields rather than
 * a union is a deliberate trade: this is an existing wire type, and a union
 * would buy compile-time safety over one runtime check at the cost of
 * reshaping every call site.
 */
export interface PluginInstallInput {
  sourceUrl?: string;
  sourceRef?: string;
  marketplace?: string;
  pluginName?: string;
}