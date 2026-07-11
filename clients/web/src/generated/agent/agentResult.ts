
import { CompletedOutput } from './completedOutput';
import { HandoffOutput } from './handoffOutput';
/**
 * The outcome of an agent run
 */
export type AgentResult =
  | { type: "Completed"; value: CompletedOutput }
  | { type: "Handoff"; value: HandoffOutput };