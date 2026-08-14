
import { CancelledResponse } from './cancelledResponse';
import { McpDiscoverResponse } from './mcpDiscoverResponse';
import { McpInvokeResponse } from './mcpInvokeResponse';
import { PongResponse } from './pongResponse';
import { ProvisionWorkspaceResponse } from './provisionWorkspaceResponse';
import { RunHooksResponse } from './runHooksResponse';
import { RuntimeReady } from './runtimeReady';
import { ScanResponse } from './scanResponse';
import { ToolCallResponse } from './toolCallResponse';
/**
 * All messages the runtime sends to the executor.
 */
export type RuntimeOutboundMessage =
  | { type: "Ready"; value: RuntimeReady }
  | { type: "ToolCallResponse"; value: ToolCallResponse }
  | { type: "ProvisionResult"; value: ProvisionWorkspaceResponse }
  | { type: "Cancelled"; value: CancelledResponse }
  | { type: "ScanResult"; value: ScanResponse }
  | { type: "HookRecords"; value: RunHooksResponse }
  | { type: "McpTools"; value: McpDiscoverResponse }
  | { type: "McpResult"; value: McpInvokeResponse }
  | { type: "Pong"; value: PongResponse };