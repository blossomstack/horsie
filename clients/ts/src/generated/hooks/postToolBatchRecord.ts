
import { PostToolBatchOutcome } from './postToolBatchOutcome';
import { ToolScope } from './toolScope';
/**
 * A whole batch of parallel calls, so it names every call rather than one.
 */
export interface PostToolBatchRecord {
  calls: ToolScope[];
  systemMessage?: string;
  outcome: PostToolBatchOutcome;
}