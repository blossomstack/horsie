
/**
 * How the session that is starting came to start.
 */
export type SessionStartSource =
  | { source: "Startup" }
  | { source: "Resume" }
  | { source: "Clear" }
  | { source: "Compact" }
  | { source: "Fork" };