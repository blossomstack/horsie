
/**
 * The hook set `continue: false`, asking horsie to stop after it.
 *
 * A common field in the spec — any hook on any event may set it, and it takes
 * precedence over `decision`. `reason` is `stopReason`, which reaches the user
 * and never the model.
 */
export interface HookHalt {
  reason?: string;
}