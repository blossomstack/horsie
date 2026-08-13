
/**
 * A typed `/compact` that had nothing to fold.
 *
 * Recorded rather than passed over in silence, which is what it used to be: a
 * command was typed and something has to answer it. The cut is the same
 * retain budget an automatic compaction uses, so a conversation already
 * inside that budget has no span to summarise — and forcing one would spend a
 * provider call and trade real messages for a summary to buy room that was
 * never scarce.
 *
 * Not a failure, and not an error: the honest answer to "compact this" is
 * sometimes "there is nothing here worth compacting".
 */
export interface CompactionSkippedLifecycle {
  /**
   * The prompt size the last provider call reported.
   */
  contextTokens: number;
  /**
   * How much of the window a compaction would have left verbatim — so the
   * notice can say what it measured against. Absent when the model's card
   * declares no context window and there is therefore no budget.
   */
  retainTokens?: number;
}