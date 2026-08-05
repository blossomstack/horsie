
import { StepParam } from './stepParam';
/**
 * A setup step the runtime executes inside its sandbox after credential
 */
export interface ProvisionStep {
  /**
   * Display label, e.g. &#34;checkout horsie&#34;.
   */
  name: string;
  /**
   * Step kind: &#34;git_checkout&#34;.
   */
  uses: string;
  /**
   * Open key/value params, interpreted per `uses`.
   */
  with: StepParam[];
}