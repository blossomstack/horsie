
import { HookBlocked } from './hookBlocked';
import { HookFailed } from './hookFailed';
import { PostToolUseRan } from './postToolUseRan';
export type PostToolUseOutcome =
  | { outcome: "Ran"; value: PostToolUseRan }
  | { outcome: "Blocked"; value: HookBlocked }
  | { outcome: "Failed"; value: HookFailed };