
/**
 * Every other event's refusal, via top-level `decision: "block"` or exit 2.
 * For `Stop` this means *blocked from stopping*, which continues the turn.
 */
export interface HookBlocked {
  reason?: string;
}