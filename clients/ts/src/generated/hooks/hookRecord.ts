
import { HookAction } from './hookAction';
import { HookHalt } from './hookHalt';
/**
 * One hook's run, as the transcript records it.
 *
 * `plugin` and `duration_ms` are the only universally true facts: every hook
 * that ran was declared by a plugin and took time. Everything else is
 * per-event and lives on the action.
 */
export interface HookRecord {
  plugin: string;
  /**
   * Wall-clock, so a hook slowing every tool call is visible.
   */
  durationMs: number;
  /**
   * Set when the hook asked horsie to stop. A common field rather than an
   * arm of every outcome union: halting is orthogonal to what the event
   * decided, and a hook that allows a call *and* halts the turn is a legal
   * reply the union shape could not express.
   */
  halt?: HookHalt;
  action: HookAction;
}