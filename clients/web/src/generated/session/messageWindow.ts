
/**
 * The first frame of a replay, describing the window that follows.
 */
export interface MessageWindow {
  /**
   * Older entries exist before this window; page back with `before=` to get
   */
  hasMoreBefore: boolean;
  /**
   * The seq of the oldest entry in this window, which is the cursor to page
   */
  earliestSeq?: number;
}