
/**
 * One plugin bundle, named and content-addressed.
 *
 * The hash is the identity: two agents selecting the same bundle name at the
 * same version resolve to one entry in the runtime's store, so the second costs
 * a symlink rather than a download.
 */
export interface BundleRef {
  name: string;
  hash: string;
}