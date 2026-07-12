
/**
 * A library entry as shown in the web UI (metadata only — never the bytes).
 */
export interface PluginView {
  /**
   * Canonical bundle name (from plugin.json, else repo basename).
   */
  name: string;
  description?: string;
  /**
   * Resolved version (manifest version, else the cloned commit sha).
   */
  version?: string;
  sourceUrl: string;
  sourceRef?: string;
  /**
   * Number of SKILL.md skills the bundle provides.
   */
  skillCount: number;
  /**
   * Whether the bundle ships a SessionStart hook.
   */
  hasHooks: boolean;
  /**
   * Pre-checked in the new-session bundle picker.
   */
  enabledDefault: boolean;
  artifactSize: number;
}