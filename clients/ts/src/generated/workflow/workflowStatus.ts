
import { AwaitingInputStatus } from './awaitingInputStatus';
import { FailedStatus } from './failedStatus';
import { FinishedStatus } from './finishedStatus';
import { PendingStatus } from './pendingStatus';
import { RunningStatus } from './runningStatus';
import { SuspendedStatus } from './suspendedStatus';
/**
 * Lifecycle of one run. `Suspended` means stopped part-way and resumable by
 * retrying a step — cancelled, or a step interrupted by a restart. `Finished`
 * carries the last step's output; `Failed` ended on an error no retry clears
 * by itself.
 */
export type WorkflowStatus =
  | { type: "Pending"; value: PendingStatus }
  | { type: "Running"; value: RunningStatus }
  | { type: "Suspended"; value: SuspendedStatus }
  | { type: "AwaitingInput"; value: AwaitingInputStatus }
  | { type: "Finished"; value: FinishedStatus }
  | { type: "Failed"; value: FailedStatus };