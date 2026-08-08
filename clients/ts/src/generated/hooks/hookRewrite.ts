
/**
 * A value a hook replaced. Both halves or neither — never a dangling "before".
 */
export interface HookRewrite {
  before: string;
  after: string;
}