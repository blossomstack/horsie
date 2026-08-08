
/**
 * A message accepted into this agent's inbox and not yet carried into a turn.
 */
export interface QueuedLifecycle {
  id: string;
  text: string;
}