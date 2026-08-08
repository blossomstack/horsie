
import { EnvironmentSpec } from '../environments';
/**
 * Start a run: the configuration creating a session takes, plus the input the
 * start step is handed.
 */
export interface WorkflowRunRequest {
  input: string;
  /**
   * Where the run's single shared runtime is built, and what it runs
   * against. Required.
   */
  environment: EnvironmentSpec;
  /**
   * Optional run title.
   */
  name?: string;
}