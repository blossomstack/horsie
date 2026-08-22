
/**
 * A sub session branched from this session. Recorded on the agent it was
 * branched *from*, because that is what a viewer is reading when it matters —
 * the same rule `SubAgentLifecycle` follows — and it marks the branch point,
 * sitting between the two messages it was taken between.
 *
 * Never reaches the model: `prompt_messages` drops every lifecycle body. That
 * is deliberate. A sub session is for the person reading, and telling the
 * source about one would disturb its prompt cache for nothing.
 *
 * `title` is what the sub session has named itself, which is nothing at the
 * moment it is created — a client reads the current one from the session list.
 *
 * `seed` is how its history was seeded: `copy`, `summary` or `fresh`.
 */
export interface SubSessionLifecycle {
  id: string;
  title?: string;
  seed: string;
}