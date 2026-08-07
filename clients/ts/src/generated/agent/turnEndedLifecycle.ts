
import { TurnOutcome } from './turnOutcome';
/**
 * A turn finished, however it finished.
 */
export interface TurnEndedLifecycle {
  outcome: TurnOutcome;
}