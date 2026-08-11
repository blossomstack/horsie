
import { StopOutcome } from './stopOutcome';
/**
 * `trigger` is the matcher domain: auto | manual.
 */
export interface PreCompactRecord {
  trigger: string;
  systemMessage?: string;
  outcome: StopOutcome;
}