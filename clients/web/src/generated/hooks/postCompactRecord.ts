
import { SideEffectOutcome } from './sideEffectOutcome';
/**
 * `trigger` is the matcher domain: auto | manual. Nothing can be decided
 * afterwards, so this reports only whether the hook ran.
 */
export interface PostCompactRecord {
  trigger: string;
  outcome: SideEffectOutcome;
}