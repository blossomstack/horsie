
import { StepRunStatus } from './stepRunStatus';
/**
 * One execution of one step. A step visited twice — by a loop or by a retry —
 */
export interface StepRunView {
  /**
   * Position in the run log; the id a retry names.
   */
  index: number;
  /**
   * Which step of the definition ran.
   */
  step: string;
  /**
   * The agent this execution is, addressable at
   */
  agentId: string;
  /**
   * 1 for the first execution of this step on this path.
   */
  attempt: number;
  status: StepRunStatus;
  /**
   * The structured output it concluded with.
   */
  output?: unknown;
  error?: string;
  startedAtMs: number;
  endedAtMs?: number;
  /**
   * Tokens this execution spent.
   */
  inputTokens: number;
  outputTokens: number;
}