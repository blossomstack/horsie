
/**
 * What a vendor can do with a session workspace. Announced by the vendor
 */
export interface VendorCapabilities {
  /**
   * The vendor provisions a workspace it owns — cloning repos, installing
   */
  supportsProvisioning: boolean;
}