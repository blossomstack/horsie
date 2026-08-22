
import { BundleGeneration } from './bundleGeneration';
import { BundleHash } from './bundleHash';
/**
 * How one revision of a bundle is named.
 */
export type BundleVersion =
  | { kind: "Hash"; value: BundleHash }
  | { kind: "Generation"; value: BundleGeneration };