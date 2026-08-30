
import { CompactionSkippedLifecycle } from '../agent';
/**
 * A compaction that was asked for and folded nothing. Carries the same
 * account the journaled lifecycle entry does, because it becomes one.
 */
export interface CompactionSkippedEvent {
  detail: CompactionSkippedLifecycle;
  atMs: number;
}