
import { HookDenied } from './hookDenied';
import { HookFailed } from './hookFailed';
import { PreToolUseAllowed } from './preToolUseAllowed';
/**
 * The only event that can refuse a call before it runs, and the only one that
 * can rewrite its input. No `additionalContext`: the spec does not offer it
 * here, and there is no result yet to attach it to.
 */
export type PreToolUseOutcome =
  | { outcome: "Allowed"; value: PreToolUseAllowed }
  | { outcome: "Denied"; value: HookDenied }
  | { outcome: "Ask" }
  | { outcome: "Defer" }
  | { outcome: "Failed"; value: HookFailed };