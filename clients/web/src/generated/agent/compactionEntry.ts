
import { CompactionTrigger } from './compactionTrigger';
/**
 * A point at which older history stopped being shown to the model.
 *
 * Nothing is deleted when one of these is appended. The log keeps every entry
 * it ever held — which is what lets a person read the whole session — and this
 * entry only moves where `AgentState::prompt_messages` *starts*. The record is
 * complete; the working set is not.
 *
 * Only the newest boundary in a log has any effect on the prompt. An older one
 * is a fact about what happened, and shows the model nothing: the span it
 * covered is already folded into the summary of every boundary that came after
 * it, so replaying it would say the same thing twice.
 */
export interface CompactionEntry {
  /**
   * The model's prose account of everything up to `covers_through_seq`.
   * Lossy by nature, and the reason `carried_state` exists separately.
   *
   * Each summary is written with the previous one already in the prompt, so
   * summarising is a fold rather than a re-derivation: no span is ever
   * summarised twice, and the cost of a compaction does not grow with the
   * number that came before it.
   */
  summary: string;
  /**
   * Facts too exact to paraphrase — the task list, armed timers, pending
   * asks, the working directory — rendered from state and never routed
   * through the summariser.
   *
   * This exists because state surviving is not the same as the model knowing
   * it survived. A task list is durable agent state, but the model's
   * awareness of it is carried entirely by the tool calls in the history
   * that a compaction summarises away. Without this an agent wakes up with
   * three open tasks and no idea it has any.
   *
   * Frozen here rather than re-rendered on every prompt build: re-rendering
   * would invalidate the provider's cache below this boundary on every
   * single turn, and it is unnecessary, because anything that changes after
   * this point arrives as a visible tool call in the retained history. This
   * is the baseline; the deltas are in plain sight.
   */
  carriedState: string;
  /**
   * The last entry folded into `summary`.
   */
  coversThroughSeq: number;
  /**
   * The first entry still shown to the model raw. Never greater than
   * `covers_through_seq + 1`, and equal to it when nothing was retained.
   *
   * The retained window overlaps what the summary already covers, which is
   * deliberate: a summary alone loses the file path or error the agent was
   * part-way through, and those live in the last few messages.
   */
  retainedFromSeq: number;
  trigger: CompactionTrigger;
  /**
   * What `/compact <instructions>` asked the summariser to keep. Absent for
   * an automatic compaction.
   */
  instructions?: string;
  /**
   * Prompt size either side of the boundary, for the transcript's own
   * account of what this bought.
   */
  tokensBefore: number;
  tokensAfter: number;
}