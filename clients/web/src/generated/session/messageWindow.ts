
/**
 * The first frame of a replay, describing the window that follows.
 *
 * A cursorless connect replays at most a capped number of the newest entries,
 * so a long-running session does not resend its whole history on every open.
 * `has_more_before` says whether the cap actually bit — the client cannot
 * infer it from the first entry's `seq`, because a log front-trimmed for
 * context management has no seq 0 either, and the two cases want opposite
 * behaviour. Sent only when there is no cursor: a resuming client already
 * knows where it is.
 */
export interface MessageWindow {
  /**
   * Older entries exist before this window; page back with `before=` to get
   * them.
   */
  hasMoreBefore: boolean;
  /**
   * The seq of the oldest entry in this window, which is the cursor to page
   * back from. Absent when the window is empty.
   */
  earliestSeq?: number;
}