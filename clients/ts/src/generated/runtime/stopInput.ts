
/**
 * `stop_hook_active` is true when horsie is only still running because a
 */
export interface StopInput {
  lastAssistantMessage?: string;
  stopHookActive: boolean;
}