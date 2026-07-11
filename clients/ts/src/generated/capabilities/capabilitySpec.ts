
import { Grant } from './grant';
import { NetworkPolicy } from './networkPolicy';
/**
 * A full sandbox capability specification. Either authored as a capability file or
 */
export interface CapabilitySpec {
  network: NetworkPolicy;
  grants: Grant[];
  /**
   * Raw platform sandbox rules (macOS Seatbelt S-expressions; ignored on Linux),
   */
  unsafeSeatbeltRules?: string[];
}