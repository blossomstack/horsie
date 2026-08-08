
/**
 * What a vendor can do with a session workspace, announced by the vendor
 * itself so the server and UI never branch on vendor name or kind.
 */
export interface RuntimeVendorCapabilities {
  /**
   * The vendor provisions a fresh workspace it owns — cloning repos,
   * installing skill bundles, running provision steps. A vendor fixed to a
   * user-owned directory announces false.
   */
  supportsProvisioning: boolean;
}