
import { SessionStartInput } from './sessionStartInput';
import { StopInput } from './stopInput';
import { SubagentStartInput } from './subagentStartInput';
import { UserPromptSubmitInput } from './userPromptSubmitInput';
/**
 * An event the server initiates, carrying that event&#39;s input.
 */
export type ServerHookEvent =
  | { event: "SessionStart"; value: SessionStartInput }
  | { event: "SubagentStart"; value: SubagentStartInput }
  | { event: "UserPromptSubmit"; value: UserPromptSubmitInput }
  | { event: "Stop"; value: StopInput };