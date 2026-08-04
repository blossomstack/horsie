
import { StepParam } from './stepParam';
/**
 * A setup step the runtime executes inside its sandbox after credential
 */
export interface ProvisionStep {
  /**
   * Display label, e.g. &quot;checkout horsie&quot;.
   */
  name: string;
  /**
   * Step kind: &quot;git_checkout&quot;.
   */
  uses: string;
  /**
   * Open key/value params, interpreted per `uses`.
   */
  with: StepParam[];
}