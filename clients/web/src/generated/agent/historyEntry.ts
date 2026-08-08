
import { HookEntry } from './hookEntry';
import { Message } from './message';
/**
 * One item in an agent's transcript.
 *
 * A transcript is not a conversation: a `Hook` entry is what a plugin *did*, and
 * only some of those put text in front of the model. `AgentState::prompt_messages`
 * translates every entry on the way to a provider, and most hook entries
 * translate to nothing — a tool hook edited the tool's own output, so the tool
 * result already carries it. Because the union sits above `Message` rather than
 * inside `ContentPart`, no provider ever holds an arm for an entry it would have
 * to interpret itself, and clients keep receiving hook entries verbatim so an
 * intervention renders as an intervention rather than as something the user said.
 */
export type HistoryEntry =
  | { type: "Llm"; value: Message }
  | { type: "Hook"; value: HookEntry };