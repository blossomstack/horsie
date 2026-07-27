import type { RenderedMessage } from "../hooks/useSessionStream";

/** The server's dedicated "ask the user" tool for sessions — kept in sync with
 * `ASK_USER_TOOL` in `server/src/sessions/ask_tool.rs`. */
export const ASK_USER_TOOL = "ask_user";

/** The `ask_user` tool call's input, as the model supplies it. */
export interface AskInput {
  question?: string;
  choices?: string[];
  multiple?: boolean;
}

export function askInputOf(input: unknown): AskInput {
  return input && typeof input === "object" ? (input as AskInput) : {};
}

/** The answer text sent to the model: picked labels joined, then any free text.
 * Plain prose on purpose — choice *indices* would leak client encoding into the
 * model's input. */
export function composeAnswer(selected: string[], text: string): string {
  const picks = selected.join(", ");
  const free = text.trim();
  if (picks && free) return `${picks}\n\n${free}`;
  return picks || free;
}

/** Best-effort recovery of which choices an answer picked, for re-rendering an
 * answered card. The selection is the answer's first block (see
 * `composeAnswer`); a label containing ", " can't be recovered, in which case
 * the chip just renders unmarked — the verbatim answer is shown either way. */
export function pickedChoices(answer: string, choices: string[]): Set<string> {
  const head = answer.split("\n\n")[0] ?? "";
  const parts = new Set(head.split(", "));
  return new Set(choices.filter((c) => parts.has(c)));
}

/** The tool call id of the ask awaiting an answer, or null. `ask_user` is
 * terminal, so only the newest ask can be pending: an older one without a
 * result belongs to an abandoned turn and must stay read-only. */
export function findPendingAsk(messages: RenderedMessage[]): string | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const calls = messages[i].toolCalls;
    for (let j = calls.length - 1; j >= 0; j--) {
      if (calls[j].name !== ASK_USER_TOOL) continue;
      return calls[j].output === undefined ? calls[j].id : null;
    }
  }
  return null;
}
