
import { PostCompactInput } from './postCompactInput';
import { PreCompactInput } from './preCompactInput';
import { SessionStartInput } from './sessionStartInput';
import { StopInput } from './stopInput';
import { SubagentStartInput } from './subagentStartInput';
import { SubagentStopInput } from './subagentStopInput';
import { UserPromptExpansionInput } from './userPromptExpansionInput';
import { UserPromptSubmitInput } from './userPromptSubmitInput';
/**
 * An event the server initiates, carrying that event's input.
 *
 * Tool events are absent by construction: they run inline in the runtime with
 * the call they guard, so asking for one out of band is unrepresentable rather
 * than merely wrong. Only wired events have arms — promoting one of the ten
 * described-but-unwired events adds its arm here alongside its call site.
 */
export type ServerHookEvent =
  | { event: "SessionStart"; value: SessionStartInput }
  | { event: "SubagentStart"; value: SubagentStartInput }
  | { event: "UserPromptSubmit"; value: UserPromptSubmitInput }
  | { event: "UserPromptExpansion"; value: UserPromptExpansionInput }
  | { event: "Stop"; value: StopInput }
  | { event: "SubagentStop"; value: SubagentStopInput }
  | { event: "PreCompact"; value: PreCompactInput }
  | { event: "PostCompact"; value: PostCompactInput };