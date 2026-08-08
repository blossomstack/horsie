
/**
 * The hook never ran to completion: spawn failure, timeout, or a non-zero exit
 * that is not a refusal. An outage, never a decision.
 */
export interface HookFailed {
  reason: string;
}