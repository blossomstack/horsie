
import { ContextInjected } from './contextInjected';
import { HookBlocked } from './hookBlocked';
import { HookFailed } from './hookFailed';
export type PostToolBatchOutcome =
  | { outcome: "Ran"; value: ContextInjected }
  | { outcome: "Blocked"; value: HookBlocked }
  | { outcome: "Failed"; value: HookFailed };