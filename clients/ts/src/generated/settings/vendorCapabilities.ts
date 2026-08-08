
/**
 * What a vendor can do with a session workspace. The new-session UI reads it
 * to decide what to prompt for, instead of branching on the vendor's name.
 *
 * Structurally identical to `runtime_vendor.RuntimeVendorCapabilities`, and
 * deliberately not the same type. That one is what a vendor process announces
 * on its own wire; this one is what the settings API reports. They agree
 * today because both have one field — unifying them would make the settings
 * package depend on the whole vendor protocol, and would tie a UI-facing
 * shape to a change in an unrelated protocol.
 */
export interface VendorCapabilities {
  /**
   * The vendor provisions a workspace it owns — cloning repos, installing
   * skill bundles, running provision steps. A vendor that runs in a fixed,
   * user-owned directory (the shared local daemon) provisions nothing.
   */
  supportsProvisioning: boolean;
}