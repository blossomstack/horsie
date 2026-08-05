
import { PostToolUseOutcome } from './postToolUseOutcome';
import { ToolScope } from './toolScope';
export interface PostToolUseRecord {
  call: ToolScope;
  systemMessage?: string;
  outcome: PostToolUseOutcome;
}