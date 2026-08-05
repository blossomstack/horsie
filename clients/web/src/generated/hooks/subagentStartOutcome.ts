
import { ContextInjected } from './contextInjected';
import { HookFailed } from './hookFailed';
export type SubagentStartOutcome =
  | { outcome: "Ran"; value: ContextInjected }
  | { outcome: "Failed"; value: HookFailed };