
import { EmptyOutcome } from './emptyOutcome';
import { FailedOutcome } from './failedOutcome';
/**
 * How a turn ended. One entry with an outcome rather than four sibling
 */
export type TurnOutcome =
  | { kind: "Ended"; value: EmptyOutcome }
  | { kind: "Failed"; value: FailedOutcome }
  | { kind: "Stopped"; value: EmptyOutcome }
  | { kind: "Interrupted"; value: EmptyOutcome };