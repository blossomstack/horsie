
import { FlyVendorSettings } from './flyVendorSettings';
import { VelosVendorSettings } from './velosVendorSettings';
/**
 * Per-kind settings. A union rather than a kind string plus optional structs,
 * so a client cannot describe a vendor that has no settings — or two kinds'
 * worth.
 *
 * Adding a kind is a variant here plus a match arm on the server. What each
 * substrate can do — Fly keeps a workspace across a hibernate, velos rebuilds
 * it — stays inside its implementation and never reaches this type.
 */
export type RuntimeVendorSettings =
  | { kind: "Fly"; value: FlyVendorSettings }
  | { kind: "Velos"; value: VelosVendorSettings };