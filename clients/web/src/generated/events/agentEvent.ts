
import { CompactedEvent } from './compactedEvent';
import { CompactionSkippedEvent } from './compactionSkippedEvent';
import { ContentBlockStopEvent } from './contentBlockStopEvent';
import { InputMessageEvent } from './inputMessageEvent';
import { MessageCompleteEvent } from './messageCompleteEvent';
import { MessageStartEvent } from './messageStartEvent';
import { MessageStopEvent } from './messageStopEvent';
import { RunAbortedEvent } from './runAbortedEvent';
import { RunCompleteEvent } from './runCompleteEvent';
import { TextBlockStartEvent } from './textBlockStartEvent';
import { TextChunkEvent } from './textChunkEvent';
import { ThinkingBlockStartEvent } from './thinkingBlockStartEvent';
import { ThinkingChunkEvent } from './thinkingChunkEvent';
import { ThinkingSignatureChunkEvent } from './thinkingSignatureChunkEvent';
import { ToolCallInputDeltaEvent } from './toolCallInputDeltaEvent';
import { ToolCallStartEvent } from './toolCallStartEvent';
import { ToolCompleteEvent } from './toolCompleteEvent';
import { ToolExecutingEvent } from './toolExecutingEvent';
/**
 * All events emitted during agent execution.
 *
 * Flow per assistant turn:
 * InputMessage (once, at the start of run)
 * MessageStart → ( block )* → MessageStop → MessageComplete
 * where each block is one of:
 * TextBlockStart     → TextChunk*                              → ContentBlockStop
 * ThinkingBlockStart → (ThinkingChunk | ThinkingSignatureChunk)* → ContentBlockStop
 * ToolCallStart      → ToolCallInputDelta*                     → ContentBlockStop
 * ToolExecuting → ToolComplete  (one pair per tool call)
 * RunComplete   (or RunAborted, if the run ended in an error instead)
 */
export type AgentEvent =
  | { type: "InputMessage"; value: InputMessageEvent }
  | { type: "MessageStart"; value: MessageStartEvent }
  | { type: "MessageStop"; value: MessageStopEvent }
  | { type: "MessageComplete"; value: MessageCompleteEvent }
  | { type: "TextBlockStart"; value: TextBlockStartEvent }
  | { type: "TextChunk"; value: TextChunkEvent }
  | { type: "ThinkingBlockStart"; value: ThinkingBlockStartEvent }
  | { type: "ThinkingChunk"; value: ThinkingChunkEvent }
  | { type: "ThinkingSignatureChunk"; value: ThinkingSignatureChunkEvent }
  | { type: "ToolCallStart"; value: ToolCallStartEvent }
  | { type: "ToolCallInputDelta"; value: ToolCallInputDeltaEvent }
  | { type: "ContentBlockStop"; value: ContentBlockStopEvent }
  | { type: "ToolExecuting"; value: ToolExecutingEvent }
  | { type: "ToolComplete"; value: ToolCompleteEvent }
  | { type: "RunComplete"; value: RunCompleteEvent }
  | { type: "RunAborted"; value: RunAbortedEvent }
  | { type: "Compacted"; value: CompactedEvent }
  | { type: "CompactionSkipped"; value: CompactionSkippedEvent };