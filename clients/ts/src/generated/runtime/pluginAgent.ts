
/**
 * An agent definition discovered in the shared plugin library: one `*.md` with
 * frontmatter naming it, and a body that is its system prompt. `rel_path` is
 * relative to the plugins root, so it identifies the file without depending on
 * where the library happens to be mounted.
 *
 * The bytes travel and the *server* parses them, exactly as skills do: the
 * runtime is the only process that can see plugin files, and reading
 * frontmatter is not file access.
 */
export interface PluginAgent {
  plugin: string;
  relPath: string;
  content: string;
}