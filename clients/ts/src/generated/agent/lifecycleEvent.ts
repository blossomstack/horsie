
import { AskLifecycle } from './askLifecycle';
import { ProvisioningLifecycle } from './provisioningLifecycle';
import { QueuedLifecycle } from './queuedLifecycle';
import { SessionFailedLifecycle } from './sessionFailedLifecycle';
import { StepLifecycle } from './stepLifecycle';
import { SubAgentLifecycle } from './subAgentLifecycle';
import { TaskListLifecycle } from './taskListLifecycle';
import { TurnBeganLifecycle } from './turnBeganLifecycle';
import { TurnEndedLifecycle } from './turnEndedLifecycle';
/**
 * Something that happened to the session, recorded in the log of the agent it
 * concerns.
 *
 * The session actor still owns every one of these; it notifies the agent so
 * there is one ordered record to read rather than a second stream to reconcile
 * against this one. Flow is one-directional: session to agent, never back.
 */
export type LifecycleEvent =
  | { kind: "Provisioning"; value: ProvisioningLifecycle }
  | { kind: "MessageQueued"; value: QueuedLifecycle }
  | { kind: "TurnBegan"; value: TurnBeganLifecycle }
  | { kind: "TurnEnded"; value: TurnEndedLifecycle }
  | { kind: "AskRecorded"; value: AskLifecycle }
  | { kind: "SubAgent"; value: SubAgentLifecycle }
  | { kind: "Step"; value: StepLifecycle }
  | { kind: "TaskList"; value: TaskListLifecycle }
  | { kind: "SessionFailed"; value: SessionFailedLifecycle };