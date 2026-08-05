
import { HookRecord } from '../hooks';
/**
 * Every hook that ran, in execution order. Injected context is derived from
 */
export interface RunHooksResponse {
  callId: string;
  records: HookRecord[];
}