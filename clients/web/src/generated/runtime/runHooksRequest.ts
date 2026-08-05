
import { ServerHookEvent } from './serverHookEvent';
/**
 * Run every matching hook for one server-initiated event inside the sandbox.
 */
export interface RunHooksRequest {
  callId: string;
  event: ServerHookEvent;
}