
import { HookRecord } from '../hooks';
/**
 * A plugin hook&#x27;s intervention, as it appears in an agent&#x27;s transcript.
 */
export interface HookEntry {
  /**
   * Cursor id, in the same space as `Message.id`. Derived from the record
   */
  id: string;
  createdAtMs: number;
  record: HookRecord;
}