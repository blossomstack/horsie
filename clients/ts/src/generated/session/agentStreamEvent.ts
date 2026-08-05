
import { AppendedEvent } from './appendedEvent';
import { DeltaEvent } from './deltaEvent';
import { ResyncEvent } from './resyncEvent';
import { TaskListEvent } from './taskListEvent';
import { ToolStartEvent } from './toolStartEvent';
import { TurnCompletedEvent } from './turnCompletedEvent';
/**
 * A frame on one agent&#39;s stream (`/sessions/:id/agents/:agent_id/events`).
 */
export type AgentStreamEvent =
  | { type: "Appended"; value: AppendedEvent }
  | { type: "TurnCompleted"; value: TurnCompletedEvent }
  | { type: "TaskListChanged"; value: TaskListEvent }
  | { type: "Delta"; value: DeltaEvent }
  | { type: "ToolStart"; value: ToolStartEvent }
  | { type: "Resync"; value: ResyncEvent };