
/**
 * A bundle cloned from git. Both external kinds carry the same fields — they
 * differ only in the packaging convention the checked-out tree follows, which
 * decides which manifest is re-read on update and whether the bundle is
 * already portable to other clients.
 */
export interface ExternalOrigin {
  url: string;
  gitRef?: string;
  /**
   * The subdirectory within the clone that is the plugin, when the source
   * named one.
   */
  subpath?: string;
  /**
   * The marketplace this bundle was picked from, when it came from one. A
   * bundle installed from a plain git URL has none.
   */
  marketplace?: string;
  /**
   * The entry's name in that marketplace, which need not match the bundle's
   * own — `42crunch-api-security-testing` installs as `api-security-testing`.
   */
  marketplaceEntry?: string;
}