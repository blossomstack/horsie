
/**
 * A conversation branched from this one. Recorded on the agent that was
 * forked, because that is what a viewer is reading when it matters — the same
 * rule `SubAgentLifecycle` follows — and it marks the branch point, sitting
 * between the two messages the fork was taken between.
 *
 * Never reaches the model: `prompt_messages` drops every lifecycle body. That
 * is deliberate. A fork is for the person reading, and telling the source
 * about one would disturb its prompt cache for nothing.
 *
 * `title` is what the fork has named itself, which is nothing at the moment it
 * is created — a client reads the current one from the session list.
 */
export interface ForkLifecycle {
  id: string;
  title?: string;
  mode: string;
}