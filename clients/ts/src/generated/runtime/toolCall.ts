
import { BashInput } from './bashInput';
import { FindAndReplaceInput } from './findAndReplaceInput';
import { GlobInput } from './globInput';
import { GrepInput } from './grepInput';
import { ListFilesInput } from './listFilesInput';
import { ReadFileInput } from './readFileInput';
import { ReplaceLinesInput } from './replaceLinesInput';
import { SetEnvInput } from './setEnvInput';
import { SetWorkingDirInput } from './setWorkingDirInput';
import { WriteFileInput } from './writeFileInput';
/**
 * One variant per tool. The tag doubles as the tool name seen by the LLM.
 */
export type ToolCall =
  | { tool: "Bash"; value: BashInput }
  | { tool: "ReadFile"; value: ReadFileInput }
  | { tool: "WriteFile"; value: WriteFileInput }
  | { tool: "FindAndReplace"; value: FindAndReplaceInput }
  | { tool: "ReplaceLines"; value: ReplaceLinesInput }
  | { tool: "ListFiles"; value: ListFilesInput }
  | { tool: "Glob"; value: GlobInput }
  | { tool: "Grep"; value: GrepInput }
  | { tool: "SetWorkingDir"; value: SetWorkingDirInput }
  | { tool: "SetEnv"; value: SetEnvInput };