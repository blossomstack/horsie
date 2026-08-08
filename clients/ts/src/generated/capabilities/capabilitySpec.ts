
import { Grant } from './grant';
import { NetworkPolicy } from './networkPolicy';
/**
 * A full sandbox capability specification. Either authored as a capability file or
 * taken from the platform built-in default; fully defines what the sandbox allows.
 */
export interface CapabilitySpec {
  network: NetworkPolicy;
  grants: Grant[];
  /**
   * Raw platform sandbox rules (macOS Seatbelt S-expressions; ignored on Linux),
   * applied verbatim after the structured grants. The CLI prepends platform defaults
   * (see `with_default_seatbelt_rules`). `unsafe_` because they bypass the structured
   * grant model.
   */
  unsafeSeatbeltRules?: string[];
}