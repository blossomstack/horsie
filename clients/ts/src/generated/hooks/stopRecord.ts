
import { StopOutcome } from './stopOutcome';
export interface StopRecord {
  systemMessage?: string;
  outcome: StopOutcome;
}