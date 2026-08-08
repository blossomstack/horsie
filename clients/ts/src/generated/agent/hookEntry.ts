
import { HookRecord } from '../hooks';
/**
 * A plugin hook's intervention, as it appears in an agent's transcript.
 */
export interface HookEntry {
  /**
   * Cursor id, in the same space as `Message.id`. Derived from the transcript
   * rather than generated (`hook:{n}`, `n` being how many hook entries precede
   * it) so journal replay produces the ids it produced the first time. A uuid
   * would give a recovered transcript different cursors than the live one.
   * Counting entries rather than records-against-a-call is what lets a
   * `SessionStart` record — which has no tool call — carry an id at all; the
   * tool join goes through the record's own `ToolScope`.
   */
  id: string;
  createdAtMs: number;
  record: HookRecord;
}