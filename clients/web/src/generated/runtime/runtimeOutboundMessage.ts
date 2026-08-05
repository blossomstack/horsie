
import { RunHooksResponse } from './runHooksResponse';
import { RuntimeProvisionFailed } from './runtimeProvisionFailed';
import { RuntimeProvisioning } from './runtimeProvisioning';
import { RuntimeReady } from './runtimeReady';
import { ScanResponse } from './scanResponse';
import { ToolCallResponse } from './toolCallResponse';
/**
 * All messages the runtime sends to the executor.
 */
export type RuntimeOutboundMessage =
  | { type: "Ready"; value: RuntimeReady }
  | { type: "Provisioning"; value: RuntimeProvisioning }
  | { type: "ProvisionFailed"; value: RuntimeProvisionFailed }
  | { type: "ToolCallResponse"; value: ToolCallResponse }
  | { type: "ScanResult"; value: ScanResponse }
  | { type: "HookRecords"; value: RunHooksResponse };