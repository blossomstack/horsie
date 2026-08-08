
import { HookRecord } from '../hooks';
/**
 * Every hook that ran, in execution order. Injected context is derived from
 * these rather than carried beside them, so no event is recorded specially.
 */
export interface RunHooksResponse {
  callId: string;
  records: HookRecord[];
}