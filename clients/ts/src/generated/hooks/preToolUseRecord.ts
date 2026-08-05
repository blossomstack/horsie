
import { PreToolUseOutcome } from './preToolUseOutcome';
import { ToolScope } from './toolScope';
export interface PreToolUseRecord {
  call: ToolScope;
  systemMessage?: string;
  outcome: PreToolUseOutcome;
}