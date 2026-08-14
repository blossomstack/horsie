
/**
 * Every request this runtime is currently executing, by call id.
 *
 * The load-bearing part is `in_flight`, not the reply itself: it turns liveness
 * from a boolean into a reconciliation against the caller's own list, which is
 * what lets a twenty-minute build be told apart from a request the runtime never
 * heard of. A boolean answer cannot distinguish them, and any deadline that
 * tried would have to bound the one thing here with no natural bound.
 */
export interface PongResponse {
  callId: string;
  inFlight: string[];
}