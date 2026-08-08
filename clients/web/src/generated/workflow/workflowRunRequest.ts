
import { RepoConfig } from '../session_api';
/**
 * Start a run: the configuration creating a session takes, plus the input the
 * start step is handed.
 */
export interface WorkflowRunRequest {
  input: string;
  /**
   * Runtime vendor for the run's single shared runtime; absent → the
   * server's default vendor at invoke.
   */
  vendor?: string;
  /**
   * Repositories cloned into the run's shared workspace.
   */
  repos?: RepoConfig[];
  /**
   * Optional run title.
   */
  name?: string;
}