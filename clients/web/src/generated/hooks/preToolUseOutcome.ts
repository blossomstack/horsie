
import { HookDenied } from './hookDenied';
import { HookFailed } from './hookFailed';
import { PreToolUseAllowed } from './preToolUseAllowed';
/**
 * The only event that can refuse a call before it runs, and the only one that
 */
export type PreToolUseOutcome =
  | { outcome: "Allowed"; value: PreToolUseAllowed }
  | { outcome: "Denied"; value: HookDenied }
  | { outcome: "Ask" }
  | { outcome: "Defer" }
  | { outcome: "Failed"; value: HookFailed };