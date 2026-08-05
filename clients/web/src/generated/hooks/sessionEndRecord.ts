
import { SideEffectOutcome } from './sideEffectOutcome';
/**
 * `reason` is the matcher domain: clear | resume | logout | prompt_input_exit | …
 */
export interface SessionEndRecord {
  reason: string;
  outcome: SideEffectOutcome;
}