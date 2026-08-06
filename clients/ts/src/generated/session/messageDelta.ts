
/**
 * A chunk of the message being written.
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