
/**
 * The handshake was refused, and no retry can change that. The only reason
 * today is a name already held by another agent; `reason` is written for the
 * human who launched the agent and never names the holder, who may be someone
 * else entirely.
 */
export interface VendorRejected {
  reason: string;
}