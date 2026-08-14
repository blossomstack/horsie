
import { ProvisionStep } from '../executor';
/**
 * Bring this runtime's workspaces to the state `steps` describes.
 *
 * A request rather than a boot phase. As a phase it had no caller: nothing
 * could time it, retry it, or run it a second time, and the only way to report
 * a failure was for the runtime to exit. It is also why `Ready` used to assert
 * three separate things at once, of which this was the one most likely to have
 * gone wrong.
 *
 * Idempotent, which is what lets the server send it on every acquisition
 * rather than remembering whether it already did. A `git_checkout` over a
 * directory that already holds that checkout does nothing. The server cannot
 * know whether a hibernated runtime kept its workspace; the runtime always can.
 */
export interface ProvisionWorkspaceRequest {
  callId: string;
  steps: ProvisionStep[];
}