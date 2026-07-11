
/**
 * Streaming text delta — live only, never journaled, never carries an SSE id.
 */
export interface DeltaEvent {
  text: string;
}