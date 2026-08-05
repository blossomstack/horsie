
import { HookRewrite } from './hookRewrite';
/**
 * `PreToolUse` allowed the call, having possibly rewritten its input. Only an
 */
export interface PreToolUseAllowed {
  input?: HookRewrite;
}