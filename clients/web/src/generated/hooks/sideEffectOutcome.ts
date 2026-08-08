
import { HookFailed } from './hookFailed';
/**
 * These support no JSON output at all — not even `systemMessage` — and cannot
 * block: exit 2 has no special meaning for them. They can still fail, and
 * their stderr is still user-facing, which is the whole of what to record.
 */
export type SideEffectOutcome =
  | { outcome: "Ran" }
  | { outcome: "Failed"; value: HookFailed };