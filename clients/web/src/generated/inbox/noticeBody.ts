
/**
 * An agent told you something and carried on. Nothing is waiting on you.
 */
export interface NoticeBody {
  /**
   * Markdown, as the agent wrote it.
   */
  body: string;
}