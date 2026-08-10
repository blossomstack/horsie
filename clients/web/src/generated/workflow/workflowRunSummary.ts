
import { SessionSummary } from '../session';
import { WorkflowStatus } from './workflowStatus';
/**
 * One past run in a workflow's list.
 *
 * The run's own `status` rather than the session's: a session reports a live
 * status only while it is loaded, and nothing is loaded at boot — so a list of
 * past runs read the session's and showed every one of them as unknown.
 * A run's lifecycle state is durable, and telling success from failure from
 * parked-on-a-question is the whole reason to look at this list.
 */
export interface WorkflowRunSummary {
  session: SessionSummary;
  status: WorkflowStatus;
}