
import { HookRecord } from '../hooks';
/**
 * A plugin hook&#39;s intervention, as it appears in an agent&#39;s transcript.
 */
export interface HookEntry {
  /**
   * Cursor id, in the same space as `Message.id`. Derived from the transcript
   */
  id: string;
  createdAtMs: number;
  record: HookRecord;
}