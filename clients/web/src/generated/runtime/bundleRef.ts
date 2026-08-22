
import { BundleVersion } from './bundleVersion';
/**
 * One plugin bundle: a name to link it under, and the version that is its
 * identity.
 *
 * Two agents selecting the same bundle at the same version resolve to one
 * entry in the runtime's store, so the second costs a symlink rather than a
 * download.
 *
 * `digest` is the sha256 of the bytes and is an integrity check, never the
 * identity. For a `Hash` version it repeats what the identity already says.
 * For a `Generation` it is the thing that catches a server whose renderer has
 * drifted from what it recorded — a case that would otherwise leave two
 * runtimes holding different trees under one name.
 */
export interface BundleRef {
  name: string;
  version: BundleVersion;
  digest: string;
}