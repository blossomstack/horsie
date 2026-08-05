
import { CancelCallRequest } from './cancelCallRequest';
import { RunHooksRequest } from './runHooksRequest';
import { ScanRequest } from './scanRequest';
import { ToolCallRequest } from './toolCallRequest';
export type RuntimeInboundMessage =
  | { type: "ToolCall"; value: ToolCallRequest }
  | { type: "CancelCall"; value: CancelCallRequest }
  | { type: "ScanWorkspace"; value: ScanRequest }
  | { type: "RunHooks"; value: RunHooksRequest };