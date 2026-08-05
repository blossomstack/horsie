
/**
 * A slash command discovered in the shared plugin library. Its *name* is the
 */
export interface PluginCommand {
  plugin: string;
  relPath: string;
  content: string;
}