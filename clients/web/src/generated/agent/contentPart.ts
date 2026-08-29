
import { ArtifactPart } from './artifactPart';
import { SubAgentResultPart } from './subAgentResultPart';
import { TextPart } from './textPart';
import { ThinkingPart } from './thinkingPart';
import { ToolCallPart } from './toolCallPart';
import { ToolResultPart } from './toolResultPart';
/**
 * Content variant within a message
 */
export type ContentPart =
  | { type: "Text"; value: TextPart }
  | { type: "ToolCall"; value: ToolCallPart }
  | { type: "ToolResult"; value: ToolResultPart }
  | { type: "Thinking"; value: ThinkingPart }
  | { type: "SubAgentResult"; value: SubAgentResultPart }
  | { type: "Artifact"; value: ArtifactPart };