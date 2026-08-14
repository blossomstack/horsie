
/**
 * The runtime abandoned this call because the caller asked it to.
 *
 * Replaces the synthetic `ToolCallResponse` a `CancelCall` used to draw. That
 * answer was tool-shaped whatever the request was, so cancelling anything else
 * resolved its waiter with "the runtime answered X with the wrong message" — a
 * protocol confusion reported in place of the cancellation that actually
 * happened. Harmless while only tool calls could be cancelled; the moment
 * every server-initiated command became cancellable it was wrong five ways.
 */
export interface CancelledResponse {
  callId: string;
}