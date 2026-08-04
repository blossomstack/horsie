
/**
 * What one plugin hook did to one tool call.
 */
export interface HookRecord {
  /**
   * The plugin that declared the hook, the event it ran for, and the tool
   */
  plugin: string;
  event: string;
  tool: string;
  /**
   * The call this record describes. The join key: without it a record cannot
   */
  toolCallId: string;
  /**
   * Wall-clock, so a hook slowing every tool call is visible.
   */
  durationMs: number;
  /**
   * The hook refused the call: exit 2, `decision: &quot;block&quot;`, or
   */
  blocked: boolean;
  reason?: string;
  /**
   * The hook could not be run to completion — spawn failure, timeout, or a
   */
  failed: boolean;
  /**
   * Set only when the hook rewrote the call&#x27;s arguments, so the UI can show
   */
  inputBefore?: string;
  inputAfter?: string;
  /**
   * Set only when the hook rewrote the result the model sees. Clamped.
   */
  outputBefore?: string;
  outputAfter?: string;
  /**
   * Injected into the model&#x27;s view of the result.
   */
  additionalContext?: string;
  /**
   * Addressed to the user, not the model.
   */
  systemMessage?: string;
}