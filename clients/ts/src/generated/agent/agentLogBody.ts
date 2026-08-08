
import { HookEntry } from './hookEntry';
import { LifecycleEvent } from './lifecycleEvent';
import { Message } from './message';
/**
 * What one log entry carries.
 *
 * Three arms, not eight: `AgentState::prompt_messages` is a match over this
 * union, and a single `Lifecycle` arm mapping to nothing covers every
 * lifecycle variant that will ever exist. Flattening the variants in here
 * would make provider isolation a per-variant obligation a future addition
 * could forget.
 */
export type AgentLogBody =
  | { type: "Llm"; value: Message }
  | { type: "Hook"; value: HookEntry }
  | { type: "Lifecycle"; value: LifecycleEvent };