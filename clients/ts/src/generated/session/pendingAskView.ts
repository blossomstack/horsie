
/**
 * One question the agent is parked on. `tool_call_id` is what an answer is
 * addressed to; it is absent only for a question journaled before call ids
 * were recorded, which can be read but not answered
 */
export interface PendingAskView {
  toolCallId?: string;
  question: string;
}