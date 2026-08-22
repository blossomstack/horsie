
/**
 * sha256 of a packed bundle. Identity *and* bytes: an external bundle is
 * content-addressed, so a store entry that exists is finished by construction.
 */
export interface BundleHash {
  hash: string;
}