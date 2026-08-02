
import { QueuedMessage } from './queuedMessage';
/**
 * The queue as it stands now, sent whole on every change — never a delta.
 */
export interface InboxChangedEvent {
  queued: QueuedMessage[];
}