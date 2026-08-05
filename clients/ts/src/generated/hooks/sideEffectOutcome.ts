
import { HookFailed } from './hookFailed';
/**
 * These support no JSON output at all — not even `systemMessage` — and cannot
 */
export type SideEffectOutcome =
  | { outcome: "Ran" }
  | { outcome: "Failed"; value: HookFailed };