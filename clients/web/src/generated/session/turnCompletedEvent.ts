
import { Usage } from '../agent';
/**
 * `at_ms` is the unix-epoch millisecond the turn completed.
 */
export interface TurnCompletedEvent {
  iterations: number;
  usage: Usage;
  atMs: number;
}