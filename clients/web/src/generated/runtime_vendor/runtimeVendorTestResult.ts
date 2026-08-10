
/**
 * What a cheap read-only call against a vendor's substrate said.
 *
 * A save already makes this call and refuses a configuration the substrate
 * rejects, so this type answers a different question: is the vendor usable
 * *now*. A credential revoked after it was stored is invisible until someone
 * asks, and the answer is deliberately not recorded — it would be a claim
 * about a remote system that can stop being true a second later.
 *
 * A failure is `ok: false` with a message, never an HTTP error: the request
 * succeeded, and what failed is the thing it went to find out about.
 */
export interface RuntimeVendorTestResult {
  ok: boolean;
  error?: string;
}