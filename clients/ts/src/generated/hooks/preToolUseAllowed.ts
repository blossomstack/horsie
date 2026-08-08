
import { HookRewrite } from './hookRewrite';
/**
 * `PreToolUse` allowed the call, having possibly rewritten its input. Only an
 * allowed call can be rewritten, which is why the rewrite lives here.
 */
export interface PreToolUseAllowed {
  input?: HookRewrite;
}