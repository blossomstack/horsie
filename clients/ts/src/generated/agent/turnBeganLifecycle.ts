
/**
 * A turn started, consuming these queued message ids and answering these asks.
 */
export interface TurnBeganLifecycle {
  consumed: string[];
  answered: string[];
}