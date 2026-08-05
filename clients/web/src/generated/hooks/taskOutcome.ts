
import { HookFailed } from './hookFailed';
export type TaskOutcome =
  | { outcome: "Ran" }
  | { outcome: "Failed"; value: HookFailed };