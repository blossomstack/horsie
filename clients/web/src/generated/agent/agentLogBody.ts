
import { CompactionEntry } from './compactionEntry';
import { HookEntry } from './hookEntry';
import { LifecycleEvent } from './lifecycleEvent';
import { Message } from './message';
/**
 * What one log entry carries.
 *
 * Four arms, not a dozen: `AgentState::prompt_messages` is a match over this
 * union, and a single `Lifecycle` arm mapping to nothing covers every
 * lifecycle variant that will ever exist. Flattening the variants in here
 * would make provider isolation a per-variant obligation a future addition
 * could forget.
 *
 * `Compaction` is the one arm whose meaning depends on where it sits: the
 * newest one decides where the prompt starts, and every older one is history
 * that shows the model nothing. See [`CompactionEntry`].
 */
export type AgentLogBody =
  | { type: "Llm"; value: Message }
  | { type: "Hook"; value: HookEntry }
  | { type: "Lifecycle"; value: LifecycleEvent }
  | { type: "Compaction"; value: CompactionEntry };