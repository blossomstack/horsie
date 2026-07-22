
import { AskedEvent } from './askedEvent';
import { DeltaEvent } from './deltaEvent';
import { ErrorEvent } from './errorEvent';
import { MessageEvent } from './messageEvent';
import { StatusChangedEvent } from './statusChangedEvent';
import { TaskListEvent } from './taskListEvent';
import { ToolOutputEvent } from './toolOutputEvent';
import { ToolStartEvent } from './toolStartEvent';
import { TurnCompletedEvent } from './turnCompletedEvent';
export type SessionEvent =
  | { type: "Message"; value: MessageEvent }
  | { type: "ToolResult"; value: ToolOutputEvent }
  | { type: "TurnCompleted"; value: TurnCompletedEvent }
  | { type: "Asked"; value: AskedEvent }
  | { type: "StatusChanged"; value: StatusChangedEvent }
  | { type: "Error"; value: ErrorEvent }
  | { type: "Delta"; value: DeltaEvent }
  | { type: "ToolStart"; value: ToolStartEvent }
  | { type: "TaskListChanged"; value: TaskListEvent };