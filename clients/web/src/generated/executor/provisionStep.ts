
import { StepParam } from './stepParam';
/**
 * A setup step the runtime executes inside its sandbox after credential
 * provisioning and before the agent message loop (e.g. cloning a repository).
 * `uses` selects the step kind — "git_checkout" is the only kind today; the
 * runtime fails on unknown kinds (fail-closed, forward-compatible).
 */
export interface ProvisionStep {
  /**
   * Display label, e.g. "checkout horsie".
   */
  name: string;
  /**
   * Step kind: "git_checkout".
   */
  uses: string;
  /**
   * Open key/value params, interpreted per `uses`.
   */
  with: StepParam[];
}