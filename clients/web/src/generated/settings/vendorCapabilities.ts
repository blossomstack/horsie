
/**
 * What a vendor can do with a session workspace. Announced by the vendor
 * itself; the new-session UI reads it to decide what to prompt for, instead
 * of branching on the vendor's name or kind.
 */
export interface VendorCapabilities {
  /**
   * The vendor provisions a workspace it owns — cloning repos, installing
   * skill bundles, running provision steps. A vendor that runs in a fixed,
   * user-owned directory (the shared local daemon) provisions nothing.
   */
  supportsProvisioning: boolean;
}