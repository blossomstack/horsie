
/**
 * The hook never ran to completion: spawn failure, timeout, or a non-zero exit
 */
export interface HookFailed {
  reason: string;
}