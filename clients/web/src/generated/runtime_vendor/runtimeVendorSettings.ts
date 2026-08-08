
import { FlyVendorSettings } from './flyVendorSettings';
/**
 * Per-kind settings. A union rather than a kind string plus optional structs,
 * so a client cannot describe a vendor that has no settings — or two kinds'
 * worth.
 */
export type RuntimeVendorSettings =
  | { kind: "Fly"; value: FlyVendorSettings };