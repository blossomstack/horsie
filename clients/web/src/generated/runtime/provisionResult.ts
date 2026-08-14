
import { ProvisionError } from './provisionError';
import { ProvisionOk } from './provisionOk';
/**
 * Fail-whole: a workspace that is only partly built is not a workspace an
 * agent can be pointed at, and every later failure would be a confusing
 * consequence of this one.
 */
export type ProvisionResult =
  | { status: "Ok"; value: ProvisionOk }
  | { status: "Err"; value: ProvisionError };