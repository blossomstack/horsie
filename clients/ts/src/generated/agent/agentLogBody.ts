
import { HookEntry } from './hookEntry';
import { LifecycleEvent } from './lifecycleEvent';
import { Message } from './message';
/**
 * What one log entry carries.
 */
export type AgentLogBody =
  | { type: "Llm"; value: Message }
  | { type: "Hook"; value: HookEntry }
  | { type: "Lifecycle"; value: LifecycleEvent };