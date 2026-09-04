
import { ApplyPatchInput } from './applyPatchInput';
import { BashInput } from './bashInput';
import { FindAndReplaceInput } from './findAndReplaceInput';
import { GlobInput } from './globInput';
import { GrepInput } from './grepInput';
import { InspectPullRequestDiffInput } from './inspectPullRequestDiffInput';
import { InspectPullRequestInput } from './inspectPullRequestInput';
import { ListFilesInput } from './listFilesInput';
import { ReadFileInput } from './readFileInput';
import { ReadImageInput } from './readImageInput';
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
  | { tool: "ReadImage"; value: ReadImageInput }
  | { tool: "WriteFile"; value: WriteFileInput }
  | { tool: "ApplyPatch"; value: ApplyPatchInput }
  | { tool: "FindAndReplace"; value: FindAndReplaceInput }
  | { tool: "ReplaceLines"; value: ReplaceLinesInput }
  | { tool: "ListFiles"; value: ListFilesInput }
  | { tool: "Glob"; value: GlobInput }
  | { tool: "Grep"; value: GrepInput }
  | { tool: "InspectPullRequest"; value: InspectPullRequestInput }
  | { tool: "InspectPullRequestDiff"; value: InspectPullRequestDiffInput }
  | { tool: "SetWorkingDir"; value: SetWorkingDirInput }
  | { tool: "SetEnv"; value: SetEnvInput };