
/**
 * The agent asked the user something and is parked on it.
 */
export interface AskLifecycle {
  toolCallId?: string;
  question: string;
}