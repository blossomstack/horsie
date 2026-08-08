
import { ContextInjected } from './contextInjected';
import { HookFailed } from './hookFailed';
/**
 * Cannot block: `SessionStart` has no decision field, because by the time it
 * runs there is nothing to refuse.
 */
export type SessionStartOutcome =
  | { outcome: "Ran"; value: ContextInjected }
  | { outcome: "Failed"; value: HookFailed };