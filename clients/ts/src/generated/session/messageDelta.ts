
/**
 * A chunk of the message being written.
 *
 * Carries the entry it follows, because a chunk means nothing without one.
 * `delta_seq` restarts at 1 after every entry, which is what makes a stale
 * position detectable: a client claiming more chunks than this run has emitted
 * was talking to a run that has since restarted, and `reset` says so.
 */
export interface MessageDelta {
  entrySeq: number;
  deltaSeq: number;
  text: string;
  /**
   * Discard the partial text you hold: these chunks start a new run.
   */
  reset: boolean;
}