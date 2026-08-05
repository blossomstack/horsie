
import { ContextInjected } from './contextInjected';
import { HookBlocked } from './hookBlocked';
import { HookFailed } from './hookFailed';
export type StopOutcome =
  | { outcome: "Ran"; value: ContextInjected }
  | { outcome: "Blocked"; value: HookBlocked }
  | { outcome: "Failed"; value: HookFailed }
  | { outcome: "CapReached"; value: HookBlocked };