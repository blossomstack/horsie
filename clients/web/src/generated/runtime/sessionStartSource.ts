
/**
 * How the session that is starting came to start.
 *
 * The spec's whole vocabulary, not the subset horsie produces. `Clear`,
 * `Compact` and `Fork` have no call site — horsie has no context compaction,
 * no fork and no clear — and are arms nothing constructs rather than values
 * that silently never appear, which is the same honesty `is_wired()` gives an
 * event horsie cannot fire. A matcher on one of them selects nothing.
 */
export type SessionStartSource =
  | { source: "Startup" }
  | { source: "Resume" }
  | { source: "Clear" }
  | { source: "Compact" }
  | { source: "Fork" };