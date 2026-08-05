
/**
 * A value a hook replaced. Both halves or neither — never a dangling &quot;before&quot;.
 */
export interface HookRewrite {
  before: string;
  after: string;
}