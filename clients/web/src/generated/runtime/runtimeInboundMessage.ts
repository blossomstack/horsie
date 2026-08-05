
import { CancelCallRequest } from './cancelCallRequest';
import { McpDiscoverRequest } from './mcpDiscoverRequest';
import { McpInvokeRequest } from './mcpInvokeRequest';
import { RunHooksRequest } from './runHooksRequest';
import { ScanRequest } from './scanRequest';
import { ToolCallRequest } from './toolCallRequest';
export type RuntimeInboundMessage =
  | { type: "ToolCall"; value: ToolCallRequest }
  | { type: "CancelCall"; value: CancelCallRequest }
  | { type: "ScanWorkspace"; value: ScanRequest }
  | { type: "RunHooks"; value: RunHooksRequest }
  | { type: "McpDiscover"; value: McpDiscoverRequest }
  | { type: "McpInvoke"; value: McpInvokeRequest };