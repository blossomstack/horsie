
/**
 * How the session that is starting came to start.
 *
 * The spec's whole vocabulary, not the subset horsie produces. `Clear`,
 * `Compact` and `Fork` have no call site — horsie has no fork and no clear,
 * and although it does compact, it deliberately does not re-fire
 * `SessionStart` afterwards: `PostCompact` already covers reacting to one, and
 * re-running every session-start hook for something that is not a session
 * start would be a second, worse way to say the same thing. They are arms
 * nothing constructs rather than values that silently never appear, which is
 * the same honesty `is_wired()` gives an event horsie cannot fire. A matcher
 * on one of them selects nothing.
 */
export type SessionStartSource =
  | { source: "Startup" }
  | { source: "Resume" }
  | { source: "Clear" }
  | { source: "Compact" }
  | { source: "Fork" };