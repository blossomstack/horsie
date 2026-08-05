
/**
 * A value a hook replaced. Both halves or neither — never a dangling &#34;before&#34;.
 */
export interface HookRewrite {
  before: string;
  after: string;
}