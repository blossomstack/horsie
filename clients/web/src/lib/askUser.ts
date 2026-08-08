
/** The server's dedicated "ask the user" tool for sessions — kept in sync with
 * `ASK_USER_TOOL` in `server/src/sessions/ask_tool.rs`. */
export const ASK_USER_TOOL = "ask_user";

/** A workflow step's terminal tool — kept in sync with `CONCLUDE_TOOL`.
 *
 * A step never gets `ask_user`: naming it beside `conclude` would stop the agent
 * loop treating `conclude` as terminal. A step asks through
 * `conclude({kind: "ask", question})` instead, which is why asking is not one
 * tool name here but two shapes. */
export const CONCLUDE_TOOL = "conclude";

/** The tool call's input, as the model supplies it. */
export interface AskInput {
  question?: string;
  choices?: string[];
  multiple?: boolean;
}

export function askInputOf(input: unknown): AskInput {
  return input && typeof input === "object" ? (input as AskInput) : {};
}

/** Whether this tool call is a question put to the user.
 *
 * `ask_user` always is. `conclude` is only when it carries a question — the same
 * tool submits a step's output, and that call is not an ask. Without this a
 * parked step's question rendered as a collapsed `conclude` row with no answer
 * box, so the run could not be unblocked from the browser at all. */
export function isAskCall(name: string, input: unknown): boolean {
  if (name === ASK_USER_TOOL) return true;
  if (name !== CONCLUDE_TOOL) return false;
  const asked = askInputOf(input);
  return typeof asked.question === "string" && asked.question.length > 0;
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
