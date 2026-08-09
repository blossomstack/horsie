
/**
 * Set the runtime vendor new sessions default to when they name none.
 *
 * Deliberately not validated against the live roster: the agent answering to
 * this name may connect long after the preference is set, and rejecting it
 * here would make the setting unusable before its agent is running.
 */
export interface DefaultRuntimeVendorInput {
  vendor: string;
}