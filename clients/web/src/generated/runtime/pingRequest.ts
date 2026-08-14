
/**
 * Asks the runtime what it is currently executing.
 *
 * A liveness call, and the only request in this protocol with a bound the
 * caller may act on: it is answered concurrently, never queued behind a running
 * tool, so an unanswered one says something a slow tool never could.
 */
export interface PingRequest {
  callId: string;
}