
/**
 * No runtime at all: the session runs with no sandbox, so it has no shell, no
 * files and no plugin skills — only the model, its MCP servers, its memory,
 * and the tools that delegate.
 *
 * A variant rather than an absent field, for the same reason the union has no
 * default: "where does this run" is always answered, and "nowhere" is one of
 * the answers rather than the shape of a question nobody asked.
 */
export interface NoRuntime {
}