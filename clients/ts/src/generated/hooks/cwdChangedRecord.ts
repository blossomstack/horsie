
import { SideEffectOutcome } from './sideEffectOutcome';
export interface CwdChangedRecord {
  cwd: string;
  outcome: SideEffectOutcome;
}