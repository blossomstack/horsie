
/**
 * A skill discovered in the shared plugin library. `rel_dir` is the skill's
 * directory relative to the plugins root, which arrives as
 * `ScanResponse.shared_root`; joining the two gives the absolute directory the
 * agent needs to read sibling resources with the filesystem tools.
 */
export interface PluginSkill {
  plugin: string;
  relDir: string;
  content: string;
}