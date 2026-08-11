
/**
 * A compaction about to happen. `trigger` is the matcher domain — `auto` or
 * `manual` — so a hook can guard the automatic case without also firing on
 * every `/compact` someone types. `instructions` is what `/compact` was given.
 *
 * Fired before any history is rewritten, which is what makes it the last
 * chance to write something down that the summary would otherwise be the only
 * record of. A hook that blocks abandons the compaction.
 */
export interface PreCompactInput {
  trigger: string;
  instructions?: string;
}