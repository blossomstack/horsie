
import { ServerHookEvent } from './serverHookEvent';
/**
 * Run every matching hook for one server-initiated event inside the sandbox.
 *
 * `agent_id` names whose plugin tree the hooks come from. A hook is a file in a
 * bundle, so an agent that selected different bundles has different hooks — and
 * an agent that selected none has none, rather than inheriting its siblings'.
 */
export interface RunHooksRequest {
  callId: string;
  agentId: string;
  event: ServerHookEvent;
}