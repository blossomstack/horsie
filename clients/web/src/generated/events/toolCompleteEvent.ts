
import { ArtifactRef } from '../agent';
/**
 * `at_ms` is the unix-epoch millisecond the tool finished. It rides on the
 * event so the tool-result message held in memory and the one folded from the
 * journal carry the identical stamp — read the clock twice and a replayed
 * transcript would disagree with the live one.
 */
export interface ToolCompleteEvent {
  messageId: string;
  toolCallId: string;
  output: string;
  isError: boolean;
  /**
   * What the result produced besides text. Defaulted because agent events
   * cross a process boundary and an older sender has no such field.
   */
  artifacts: ArtifactRef[];
  atMs: number;
}