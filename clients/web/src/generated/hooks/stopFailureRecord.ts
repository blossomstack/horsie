
import { SideEffectOutcome } from './sideEffectOutcome';
/**
 * `error` is the matcher domain: rate_limit | overloaded | … | unknown
 */
export interface StopFailureRecord {
  error: string;
  outcome: SideEffectOutcome;
}