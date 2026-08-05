
import { HookAction } from './hookAction';
/**
 * One hook&#39;s run, as the transcript records it.
 */
export interface HookRecord {
  plugin: string;
  /**
   * Wall-clock, so a hook slowing every tool call is visible.
   */
  durationMs: number;
  action: HookAction;
}