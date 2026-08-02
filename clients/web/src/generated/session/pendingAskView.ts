
/**
 * One question the agent is parked on. `tool_call_id` is what an answer is
 */
export interface PendingAskView {
  toolCallId?: string;
  question: string;
}