
import { StoppedCall } from './stoppedCall';
/**
 * A tool ended the run. `submit_result` is always one call; `ask_user` may be
 * several issued in the same turn, and all of them are answered together
 */
export interface StoppedOutput {
  calls: StoppedCall[];
}