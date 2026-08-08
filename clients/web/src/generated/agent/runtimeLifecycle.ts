
import { RuntimeStatus } from './runtimeStatus';
/**
 * Whether the session's sandbox exists. A fact about the session, not
 * narration: an agent may not start a turn without a runtime, so this is the
 * notification that lets one begin.
 */
export interface RuntimeLifecycle {
  status: RuntimeStatus;
  detail?: string;
}