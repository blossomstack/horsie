
import { CancelCallRequest } from './cancelCallRequest';
import { McpDiscoverRequest } from './mcpDiscoverRequest';
import { McpInvokeRequest } from './mcpInvokeRequest';
import { PingRequest } from './pingRequest';
import { ProvisionAgentRequest } from './provisionAgentRequest';
import { ProvisionWorkspaceRequest } from './provisionWorkspaceRequest';
import { RunHooksRequest } from './runHooksRequest';
import { ScanRequest } from './scanRequest';
import { ToolCallRequest } from './toolCallRequest';
export type RuntimeInboundMessage =
  | { type: "ToolCall"; value: ToolCallRequest }
  | { type: "CancelCall"; value: CancelCallRequest }
  | { type: "ScanWorkspace"; value: ScanRequest }
  | { type: "RunHooks"; value: RunHooksRequest }
  | { type: "McpDiscover"; value: McpDiscoverRequest }
  | { type: "McpInvoke"; value: McpInvokeRequest }
  | { type: "Ping"; value: PingRequest }
  | { type: "ProvisionWorkspace"; value: ProvisionWorkspaceRequest }
  | { type: "ProvisionAgent"; value: ProvisionAgentRequest };