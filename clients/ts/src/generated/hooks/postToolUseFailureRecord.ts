
import { PostToolUseFailureOutcome } from './postToolUseFailureOutcome';
import { ToolScope } from './toolScope';
export interface PostToolUseFailureRecord {
  call: ToolScope;
  systemMessage?: string;
  outcome: PostToolUseFailureOutcome;
}