
import { CancelCallRequest } from './cancelCallRequest';
import { ScanRequest } from './scanRequest';
import { SessionStartRequest } from './sessionStartRequest';
import { ToolCallRequest } from './toolCallRequest';
export type RuntimeInboundMessage =
  | { type: "ToolCall"; value: ToolCallRequest }
  | { type: "CancelCall"; value: CancelCallRequest }
  | { type: "ScanWorkspace"; value: ScanRequest }
  | { type: "SessionStart"; value: SessionStartRequest };