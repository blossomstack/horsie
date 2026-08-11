
import { EmptyOutcome } from './emptyOutcome';
/**
 * Why a compaction happened. Two arms rather than a bool so a third reason
 * (a provider overflow, say) is an arm rather than a second flag.
 */
export type CompactionTrigger =
  | { kind: "Auto"; value: EmptyOutcome }
  | { kind: "Manual"; value: EmptyOutcome };