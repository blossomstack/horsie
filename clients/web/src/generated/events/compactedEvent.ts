
import { CompactionTrigger } from '../agent';
/**
 * Older history stopped being shown to the model. `message_id` is the run this
 * happened inside, or a fresh uuid for a `/compact` that ran on its own.
 *
 * Emitted from the top of the tool loop, which is the only point in a run
 * where every `tool_use` already has its `tool_result` — a boundary taken
 * anywhere else could split a call from its answer.
 *
 * Carries a message *id* rather than the `CompactionEntry`'s sequence numbers
 * because the two live in different numbering. A run holds a `Vec<Message>`
 * built by `prompt_messages`, which drops lifecycle entries and translates
 * hooks, so the nth message is not the nth log entry. The id is the one name
 * both sides share, and the fold — which is the only thing holding the log —
 * resolves it to a seq. Resolution is a search of an append-only log, so
 * replay reproduces it exactly.
 */
export interface CompactedEvent {
  messageId: string;
  summary: string;
  carriedState: string;
  /**
   * Id of the first message still shown to the model raw. Absent when
   * nothing was retained and the compaction is summary-only.
   */
  retainedFromMessageId?: string;
  trigger: CompactionTrigger;
  instructions?: string;
  tokensBefore: number;
  tokensAfter: number;
  atMs: number;
}