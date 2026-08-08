
/**
 * `stop_hook_active` is true when horsie is only still running because a
 * previous `Stop` hook blocked. A cooperative hook returns early rather than
 * looping; the server's hard cap exists for the hooks that do not.
 */
export interface StopInput {
  lastAssistantMessage?: string;
  stopHookActive: boolean;
}