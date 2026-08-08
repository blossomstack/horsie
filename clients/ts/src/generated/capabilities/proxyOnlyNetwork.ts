
/**
 * Outbound TCP restricted to `localhost:<port>` only — all other egress is
 * kernel-blocked. Confines an agent to a local policy proxy. Unix-socket grants
 * (e.g. the runtime↔executor IPC socket) are a separate capability layer in nono
 * and remain in effect. `port` is `u32` because the schema language has no `u16`;
 * the runtime rejects values above 65535 at apply time.
 */
export interface ProxyOnlyNetwork {
  port: number;
}