
import { HookEntry } from './hookEntry';
import { Message } from './message';
/**
 * One item in an agent&#x27;s transcript.
 */
export type HistoryEntry =
  | { type: "Llm"; value: Message }
  | { type: "Hook"; value: HookEntry };