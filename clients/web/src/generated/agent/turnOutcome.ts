
import { EmptyOutcome } from './emptyOutcome';
import { FailedOutcome } from './failedOutcome';
/**
 * How a turn ended. One entry with an outcome rather than four sibling
 * lifecycle variants: every consumer that cares about "the turn is over"
 * should not have to enumerate the ways.
 */
export type TurnOutcome =
  | { kind: "Ended"; value: EmptyOutcome }
  | { kind: "Failed"; value: FailedOutcome }
  | { kind: "Stopped"; value: EmptyOutcome }
  | { kind: "Interrupted"; value: EmptyOutcome };