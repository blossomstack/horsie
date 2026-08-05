
import { ContextInjected } from './contextInjected';
import { HookBlocked } from './hookBlocked';
import { HookFailed } from './hookFailed';
export type UserPromptSubmitOutcome =
  | { outcome: "Ran"; value: ContextInjected }
  | { outcome: "Blocked"; value: HookBlocked }
  | { outcome: "Failed"; value: HookFailed };