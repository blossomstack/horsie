
/**
 * An agent asked you something and stopped until it has an answer.
 */
export interface AskBody {
  question: string;
  /**
   * Suggested answers. The person may always reply in their own words, so
   * these are never a constraint — the same contract `ask_user` states to
   * the model.
   */
  choices: string[];
  /**
   * Whether several of `choices` may be picked at once. Meaningless without
   * them.
   */
  multiple: boolean;
  /**
   * The parked `tool_use` this answers. It is the address an answer is sent
   * to, and it is what makes an ask row idempotent to re-assert: one row per
   * dangling call, however many times the projection runs.
   */
  toolCallId: string;
}