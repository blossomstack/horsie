
import { AgentInput } from '../agent';
/**
 * Input received by the agent — one per run() invocation
 */
export interface InputMessageEvent {
  messageId: string;
  input: AgentInput;
}