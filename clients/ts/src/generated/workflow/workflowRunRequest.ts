
import { RepoConfig } from '../session_api';
/**
 * Start a run: the configuration creating a session takes, plus the input the
 */
export interface WorkflowRunRequest {
  input: string;
  /**
   * Runtime vendor for the run&#39;s single shared runtime; absent → the
   */
  vendor?: string;
  /**
   * Repositories cloned into the run&#39;s shared workspace.
   */
  repos?: RepoConfig[];
  /**
   * Optional run title.
   */
  name?: string;
}