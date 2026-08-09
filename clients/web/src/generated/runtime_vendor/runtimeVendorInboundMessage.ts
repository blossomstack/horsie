
import { RuntimeVendorCommand } from './runtimeVendorCommand';
export interface RuntimeVendorInboundMessage {
  requestId: string;
  command: RuntimeVendorCommand;
}