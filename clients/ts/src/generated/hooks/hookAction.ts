
import { CwdChangedRecord } from './cwdChangedRecord';
import { NotificationRecord } from './notificationRecord';
import { PostToolBatchRecord } from './postToolBatchRecord';
import { PostToolUseFailureRecord } from './postToolUseFailureRecord';
import { PostToolUseRecord } from './postToolUseRecord';
import { PreToolUseRecord } from './preToolUseRecord';
import { SessionEndRecord } from './sessionEndRecord';
import { SessionStartRecord } from './sessionStartRecord';
import { StopFailureRecord } from './stopFailureRecord';
import { StopRecord } from './stopRecord';
import { SubagentStartRecord } from './subagentStartRecord';
import { SubagentStopRecord } from './subagentStopRecord';
import { TaskCompletedRecord } from './taskCompletedRecord';
import { TaskCreatedRecord } from './taskCreatedRecord';
import { UserPromptSubmitRecord } from './userPromptSubmitRecord';
/**
 * What one hook did, tagged by the event it ran for.
 */
export type HookAction =
  | { event: "PreToolUse"; value: PreToolUseRecord }
  | { event: "PostToolUse"; value: PostToolUseRecord }
  | { event: "PostToolUseFailure"; value: PostToolUseFailureRecord }
  | { event: "PostToolBatch"; value: PostToolBatchRecord }
  | { event: "SessionStart"; value: SessionStartRecord }
  | { event: "SessionEnd"; value: SessionEndRecord }
  | { event: "UserPromptSubmit"; value: UserPromptSubmitRecord }
  | { event: "Stop"; value: StopRecord }
  | { event: "StopFailure"; value: StopFailureRecord }
  | { event: "SubagentStart"; value: SubagentStartRecord }
  | { event: "SubagentStop"; value: SubagentStopRecord }
  | { event: "TaskCreated"; value: TaskCreatedRecord }
  | { event: "TaskCompleted"; value: TaskCompletedRecord }
  | { event: "Notification"; value: NotificationRecord }
  | { event: "CwdChanged"; value: CwdChangedRecord };