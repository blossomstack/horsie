
import { HookAction } from './hookAction';
import { HookHalt } from './hookHalt';
/**
 * One hook&#39;s run, as the transcript records it.
 */
export interface HookRecord {
  plugin: string;
  /**
   * Wall-clock, so a hook slowing every tool call is visible.
   */
  durationMs: number;
  /**
   * Set when the hook asked horsie to stop. A common field rather than an
   */
  halt?: HookHalt;
  action: HookAction;
}