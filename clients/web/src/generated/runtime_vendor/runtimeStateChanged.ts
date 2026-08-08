
import { RuntimeState } from '../executor';
/**
 * A runtime changed state on its own — it died, or finished stopping.
 *
 * Distinct from the lifecycle responses above, which answer something the
 * server asked for. This one arrives unbidden, and is the server's only
 * warning that a runtime is gone while its vendor is still connected.
 */
export interface RuntimeStateChanged {
  runtimeId: string;
  state: RuntimeState;
}