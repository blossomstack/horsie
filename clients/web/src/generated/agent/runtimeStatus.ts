
import { EmptyOutcome } from './emptyOutcome';
/**
 * The three states a session's sandbox can be in from an agent's point of
 * view. Typed rather than a stage string because a consumer acts on it, and a
 * label shared with turn-preparation progress could not be told apart.
 */
export type RuntimeStatus =
  | { kind: "Acquiring"; value: EmptyOutcome }
  | { kind: "Ready"; value: EmptyOutcome }
  | { kind: "Failed"; value: EmptyOutcome };