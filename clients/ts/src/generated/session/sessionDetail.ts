
import { SessionStatusKind } from './sessionStatusKind';
export interface SessionDetail {
  id: string;
  name?: string;
  status: SessionStatusKind;
  createdAt: number;
  lastError?: string;
  /**
   * The question the agent is awaiting an answer to (status AwaitingInput).
   */
  pendingQuestion?: string;
  model: string;
  workdirs: string[];
  vendor: string;
  /**
   * Clone URLs of the session&#x27;s provisioned repos (empty for workdir sessions).
   */
  repos: string[];
}